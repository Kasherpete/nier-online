//! Player sync: capture the local character, and spawn and drive the remote one.
//!
//! The remote player is a spawned `Pl0000` made the local player's buddy (AutomataMP
//! `NierClient::on_create_player` / `Player.cpp`; sequence as refined by nier_coop `hooks/spawn.rs`). Her
//! buddy AI is overridden every frame: she is driven through her controller fields (stick, held and
//! pressed buttons) plus replayed `start_animation` calls, so the engine plays its own locomotion and
//! attacks, and her position is corrected toward the interpolated network position.

use std::ffi::{CStr, c_void};
use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering::Relaxed};

use crate::hooks::{self, game};
use crate::net::protocol::{Event, PlayerSnapshot};
use crate::sdk::{self, Entity, Pl0000, SpawnMatrix, SpawnParams, cVec4};
use crate::state::{self, Q};
use crate::sync::interp::{self, Timeline};
use crate::sync::world;

/// Error above which the remote snaps to her network position instead of blending (metres).
const SNAP_DISTANCE: f32 = 3.0;
/// Within this distance of the aim point an entity counts as aimed at (pod targeting).
const AIM_RADIUS: f32 = 4.0;
/// Pause between spawn attempts, so a failing spawn is not retried every frame.
const SPAWN_RETRY_MS: f64 = 5000.0;
/// `muteki_timer` value that keeps her invincible (what `Pl0000.setEventMuteki` sets).
const MUTEKI: f32 = 300.0;

// Shared with the hooks, which may run inside game calls we make (so no locks): this frame's validated
// local player and remote player behaviors (0 = none), and the remote's inputs.
static PLAYER: AtomicUsize = AtomicUsize::new(0);
static REMOTE: AtomicUsize = AtomicUsize::new(0);
static REMOTE_HELD: AtomicU32 = AtomicU32::new(0);
static REMOTE_FIRING: AtomicBool = AtomicBool::new(false);
static REMOTE_AIM: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];
/// A session is up: record local animations for the peer.
static SENDING: AtomicBool = AtomicBool::new(false);

pub enum RemoteEvent {
    Anim([u32; 4]),
    Buttons([u32; 8]),
}

#[derive(Default)]
pub struct PlayerSync {
    pub timeline: Timeline<RemoteEvent>,
    /// Handle of the spawned remote character (0 = none).
    spawned: u32,
    /// The local player's buddy before we replaced it, restored when the session ends.
    original_buddy: u32,
    next_spawn_ms: f64,
    /// A spawn with the peer's model failed: its assets may not be loaded here, so use 2B.
    fallback_2b: bool,
    last_local: PlayerSnapshot,
}

fn now_u32(now: f64) -> u32 {
    now as u32
}

impl PlayerSync {
    pub fn new() -> Self {
        Self { timeline: Timeline::new(), ..Default::default() }
    }

    pub fn remote_ptr(&self) -> Option<*mut Pl0000> {
        world::entity_by_handle(self.spawned).and_then(world::as_pl0000)
    }

    /// Called once per frame by `sync::tick`.
    pub fn tick(&mut self, now: f64, dt: f32, active: bool) {
        SENDING.store(active, Relaxed);
        let local = world::player();
        PLAYER.store(local.map_or(0, |(_, p)| p as usize), Relaxed);
        self.capture_local(local.map(|(_, p)| p), now, active);

        let remote = self.remote_ptr();
        if remote.is_none() {
            self.spawned = 0;
        }
        REMOTE.store(0, Relaxed);
        if !active {
            self.despawn(local);
            return;
        }
        let (Some((local_handle, pl)), false) = (local, self.timeline.is_stale(now)) else {
            return; // keep her (invincible, standing) until the peer comes back or the session ends
        };
        let Some(b) = remote.or_else(|| self.try_spawn(now, local_handle, pl)) else { return };
        REMOTE.store(b as usize, Relaxed);
        self.drive(b, now, dt);
    }

    fn capture_local(&mut self, pl: Option<*mut Pl0000>, now: f64, active: bool) {
        let t = now_u32(now);
        let Some(pl) = pl else {
            // Not a Pl0000 this frame (hacking, flight unit, menus): tell the peer to hold her still.
            self.last_local = PlayerSnapshot { time_ms: t, flags: 0, speed: 0.0, held: 0, ..self.last_local };
            state::LOCAL_PLAYER.store(&self.last_local);
            return;
        };
        // SAFETY: `pl` is a validated Pl0000 of the live entity list.
        unsafe {
            let b = &(*pl).base;
            let buttons = addr_of!((*pl).buttons).read();
            if active && buttons.iter().any(|&x| x != 0) {
                state::send_event(Event::Buttons { time_ms: t, buttons });
            }
            let mut flags = PlayerSnapshot::FLAG_PL0000;
            if addr_of!((*pl).flashlight).read() != 0 {
                flags |= PlayerSnapshot::FLAG_FLASHLIGHT;
            }
            self.last_local = PlayerSnapshot {
                time_ms: t,
                flags,
                pos: addr_of!(b.position).read(),
                rot: addr_of!(b.rotation).read(),
                facing2: addr_of!((*pl).facing2).read(),
                speed: addr_of!((*pl).speed).read(),
                held: addr_of!((*pl).held_flags).read(),
                hp: addr_of!(b.health).read(),
                aim: addr_of!((*pl).pod_aim).read(),
                pod_cooldown: world::pod_cooldown_slot(pl).map_or(0.0, |c| c.read()),
                model: addr_of!(b.model_index).read(),
            };
        }
        state::LOCAL_PLAYER.store(&self.last_local);
    }

    /// Spawns the remote character next to `pl` and makes her `pl`'s buddy. Sequence from AutomataMP
    /// (spawn, link buddies, unsuspend, player routine, `changePlayer` twice), plus nier_coop's
    /// `setBuddyFromNpc` with `obj_flags = -1`, which settles her player/buddy state.
    fn try_spawn(&mut self, now: f64, pl_handle: u32, pl: *mut Pl0000) -> Option<*mut Pl0000> {
        if now < self.next_spawn_ms {
            return None;
        }
        self.next_spawn_ms = now + SPAWN_RETRY_MS;
        let peer = self.timeline.sample(now)?.base;
        let (from_npc, change) = (world::script_fn("Pl0000.setBuddyFromNpc")?, world::script_fn("Pl0000.changePlayer")?);
        let g = game();
        let model = if peer.model == 0 || self.fallback_2b { sdk::model::PL_2B } else { peer.model };
        // SAFETY: `pl` is a validated Pl0000; the spawn function and script natives have these signatures.
        unsafe {
            let m = SpawnMatrix::at(addr_of!((*pl).base.position).read(), addr_of!((*pl).base.rotation).read());
            let name: &CStr = c"partner";
            let params = SpawnParams::new(name, model, &m);
            let e = hooks::call_spawn(g.addr.abs(g.addr.spawner) as *mut c_void, &params);
            if e.is_null() {
                tracing::error!("remote player spawn (model {model:#x}) returned null; retrying in 5 s");
                self.fallback_2b = true;
                return None;
            }
            let Some(b) = world::as_pl0000(e) else {
                tracing::error!("spawned remote player has no Pl0000 behavior yet");
                return None;
            };
            let h = world::entity_handle(e);
            self.original_buddy = addr_of!((*pl).buddy_handle).read();
            addr_of_mut!((*b).buddy_handle).write(pl_handle);
            addr_of_mut!((*pl).buddy_handle).write(h);
            world::set_suspend(b.cast(), false);
            world::assign_routine(e, c"PLAYER");
            world::assign_routine(e, c"player");
            let change = std::mem::transmute::<usize, unsafe extern "system" fn(*mut Pl0000)>(change);
            change(pl);
            change(pl);
            addr_of_mut!((*b).base.obj_flags).write(u32::MAX);
            std::mem::transmute::<usize, unsafe extern "system" fn(*mut Pl0000)>(from_npc)(b);
            addr_of_mut!((*b).base.obj_flags).write(0);
            self.spawned = h;
            tracing::info!("spawned remote player (model {model:#x}, handle {h:#x})");
            Some(b)
        }
    }

    /// Session over: remove her and give the local player back the buddy they had.
    fn despawn(&mut self, local: Option<(u32, *mut Pl0000)>) {
        if self.spawned == 0 {
            return;
        }
        if let Some(b) = self.remote_ptr() {
            world::terminate(b.cast());
            tracing::info!("removed remote player");
        }
        if let Some((_, pl)) = local {
            let restore = if world::entity_by_handle(self.original_buddy).is_some() { self.original_buddy } else { 0 };
            // SAFETY: validated Pl0000.
            unsafe { addr_of_mut!((*pl).buddy_handle).write(restore) };
        }
        self.spawned = 0;
        self.timeline.clear();
    }

    fn drive(&mut self, b: *mut Pl0000, now: f64, dt: f32) {
        let Some(s) = self.timeline.sample(now) else { return };
        let peer = s.base;
        let in_pl = peer.flags & PlayerSnapshot::FLAG_PL0000 != 0;
        // SAFETY: `b` is the validated remote Pl0000.
        unsafe {
            // Her death would fail the mission, and her real damage happens on the peer's machine.
            addr_of_mut!((*b).muteki_timer).write(MUTEKI);
            addr_of_mut!((*b).base.health).write(peer.hp.max(1));
            addr_of_mut!((*b).flashlight).write((peer.flags & PlayerSnapshot::FLAG_FLASHLIGHT != 0) as u8);

            let pos = addr_of_mut!((*b).base.position);
            pos.write(interp::correct(pos.read(), s.pos, dt, SNAP_DISTANCE));
            addr_of_mut!((*b).base.rotation).write(s.rot);

            // Controller inputs: the engine turns these into walk/run/stop and attacks on its own.
            addr_of_mut!((*b).run_speed_type).write(0);
            addr_of_mut!((*b).facing2).write(peer.facing2);
            addr_of_mut!((*b).speed).write(if in_pl { peer.speed } else { 0.0 });
        }
        let mut held = if in_pl { peer.held } else { 0 };
        for e in self.timeline.due_events(now) {
            match e {
                RemoteEvent::Buttons(buttons) if in_pl => {
                    // SAFETY: as above.
                    unsafe { addr_of_mut!((*b).buttons).write(buttons) };
                    held |= buttons.iter().enumerate().filter(|(_, v)| **v != 0).fold(0, |h, (i, _)| h | 1 << i);
                }
                RemoteEvent::Anim(args) if in_pl => {
                    if !sdk::ANIM_FROM_INPUT.contains(&args[0]) && !sdk::ANIM_CRASHES.contains(&args[0]) {
                        hooks::call_start_animation(b.cast(), args[0], args[1], args[2], args[3]);
                    }
                }
                _ => {}
            }
        }
        REMOTE_HELD.store(held, Relaxed);
        set_held(b, held); // also forced in `on_held`, after the engine rebuilds held_flags from her AI
        let firing = held & sdk::HELD_POD_FIRE != 0;
        REMOTE_FIRING.store(firing, Relaxed);
        for (a, v) in REMOTE_AIM.iter().zip([peer.aim.x, peer.aim.y, peer.aim.z]) {
            a.store(v.to_bits(), Relaxed);
        }
        if firing {
            // Her pod aims at a target entity, not a camera ray: use what sits at the peer's aim point.
            let t = nearest_entity(peer.aim, b).unwrap_or(0);
            // SAFETY: as above.
            unsafe { addr_of_mut!((*b).pod_target).write(t) };
        }
        run_pod_program(b, peer.pod_cooldown);
    }

    /// Feeds network input into the timeline (called by `sync::tick` while draining queues).
    pub fn on_snapshot(&mut self, at_ms: f64, s: PlayerSnapshot) {
        self.timeline.push_snapshot(at_ms, s);
    }

    pub fn reset(&mut self) {
        self.timeline.clear();
    }
}

/// The remote counts as AI-controlled, so her pod reads `ai_pod_fire` instead of the pod-fire held bit.
fn set_held(b: *mut Pl0000, held: u32) {
    // SAFETY: validated Pl0000.
    unsafe {
        addr_of_mut!((*b).held_flags).write(held);
        addr_of_mut!((*b).ai_pod_fire).write((held & sdk::HELD_POD_FIRE != 0) as i32);
    }
}

/// Handle of the entity closest to `aim` (within `AIM_RADIUS`), skipping `skip`. None means free aim.
fn nearest_entity(aim: cVec4, skip: *mut Pl0000) -> Option<u32> {
    let mut best = (AIM_RADIUS, None);
    for (h, e) in world::entities() {
        let Some(b) = world::as_app_base(e) else { continue };
        if b.cast() == skip {
            continue;
        }
        // SAFETY: validated BehaviorAppBase.
        let d = unsafe { addr_of!((*b).position).read() }.dist(aim);
        if d < best.0 {
            best = (d, Some(h));
        }
    }
    best.1
}

/// The engine runs the pod-program update only for the player-controlled character, so the remote's skill
/// press never starts a program; run it for her, behind the gates the engine applies to everyone
/// (nier_coop `run_pod_program`). The cooldown only ticks in the player-only routine, so copy the peer's.
fn run_pod_program(b: *mut Pl0000, peer_cooldown: f32) {
    if let Some(c) = world::pod_cooldown_slot(b) {
        // SAFETY: slot inside the validated Pl0000.
        unsafe { c.write(peer_cooldown) };
    }
    let state: u32 = world::pl_read(b, sdk::PL_STATE);
    if sdk::POD_PROGRAM_BLOCKERS.iter().any(|&o| world::pl_read::<u32>(b, o) != 0)
        || world::pl_read::<u32>(b, sdk::POD_PROGRAM_FLAGS) & 0x20 != 0
        || state == 0x41
        || state == 0x46
    {
        return;
    }
    let g = game();
    // SAFETY: resolved `fn(*mut Pl0000)`, called for a validated Pl0000 on the game thread.
    unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(*mut Pl0000)>(g.addr.abs(g.addr.pod_program))(b) }
}

// -------------------------------------------------------------------------------------------------------
// Hook callbacks (no locks: they can run inside game calls made from `tick`)

/// `Pl0000::start_animation` detour.
pub fn on_start_animation(this: *mut c_void, anim: u32, variant: u32, a3: u32, a4: u32) {
    let p = this as usize;
    if p != 0 {
        if p == REMOTE.load(Relaxed) && anim == sdk::ANIM_PARTNER_WAIT {
            return; // her buddy AI's idle; she mirrors the peer instead
        }
        if p == PLAYER.load(Relaxed) && SENDING.load(Relaxed) {
            let _ = Q.out_events.push(Event::Anim { time_ms: now_u32(state::now_ms()), args: [anim, variant, a3, a4] });
        }
    }
    hooks::call_start_animation(this, anim, variant, a3, a4);
}

/// Mid-hook after the engine rebuilt held_flags from its input source (pad or AI); for the remote, use the
/// peer's. `ctrl` is the CharacterController (`Pl0000 + 0xCA0`).
pub fn on_held(ctrl: usize) {
    let b = REMOTE.load(Relaxed);
    if b != 0 && ctrl.wrapping_sub(sdk::CONTROLLER_OFFSET) == b {
        set_held(b as *mut Pl0000, REMOTE_HELD.load(Relaxed));
    }
}

/// Mid-hook in pod_shot where a non-player pod found no target: for the remote while the peer fires, write
/// the peer's aim point into pod_shot's aim vector (`out`) and continue at the aim copy, as if a target
/// were there. Returns where to continue, or None to run the original code.
pub fn on_pod_free_aim(pl: usize, out: usize) -> Option<usize> {
    let b = REMOTE.load(Relaxed);
    if b == 0 || pl != b || out == 0 || !REMOTE_FIRING.load(Relaxed) {
        return None;
    }
    let aim = cVec4::new(
        f32::from_bits(REMOTE_AIM[0].load(Relaxed)),
        f32::from_bits(REMOTE_AIM[1].load(Relaxed)),
        f32::from_bits(REMOTE_AIM[2].load(Relaxed)),
        1.0,
    );
    // SAFETY: `out` is pod_shot's own stack slot for the aim vector ([rbp-0x60], see addr POD_NO_TARGET).
    unsafe { (out as *mut cVec4).write_unaligned(aim) };
    let g = game();
    Some(g.addr.abs(g.addr.pod_aim_copy))
}

/// The remote player's head position, for the name tag.
pub fn head_position(b: *mut Pl0000) -> [f32; 3] {
    // SAFETY: validated Pl0000.
    let p = unsafe { addr_of!((*b).base.position).read() };
    [p.x, p.y + 1.9, p.z]
}

/// Used by entity sync to skip the remote player.
pub fn is_remote(e: *mut Entity) -> bool {
    // SAFETY: entities from the live list.
    let b = unsafe { addr_of!((*e).behavior).read() } as usize;
    b != 0 && b == REMOTE.load(Relaxed)
}
