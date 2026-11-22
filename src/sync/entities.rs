//! Enemy sync: the host owns enemies, the client mirrors them.
//!
//! Design from AutomataMP `src/mods/multiplayer/EntitySync.cpp` (host announces spawns, streams state,
//! client spawns copies) combined with the newer nier_coop `hooks/world.rs` / `hooks/enemy_anim.rs` (pair the
//! client's own enemies with the host's, and block the client's enemy AI from starting animations).
//!
//! Host:
//! - Every networkable entity gets a net id. Spawns are announced with `SpawnEntity` (and every live one on
//!   connect), state goes out at 20 Hz in `Datagram::Entities`.
//! - The `EmBase::start_animation` detour records each enemy's animation calls; they are sent as `EntityAnim`.
//!   A die state, or HP staying at 0 for a second, is sent once as `EntityDied`; leaving the entity list as
//!   `DestroyEntity`.
//!
//! Client:
//! - **Pairing first.** A host entity is backed by one of our own game's entities of the same model when
//!   there is one nearby (or the only one of that model on both sides, at any distance: bosses). Our own
//!   entities keep what our scripts know about them (quest triggers, boss parts, phase scripts), which a
//!   spawned copy would not have. Only when nothing matches within `PAIR_GRACE_MS` do we spawn a copy, and
//!   a copy is swapped for our own entity if one turns up later. Our own entities that match nothing the host
//!   has for `LOCAL_GRACE_MS` are removed (AutomataMP: "deletes enemies ... that are not supposed to be
//!   networked").
//! - **The host's AI decides.** Backing enemies are in [`BLOCKED`]: our AI's `start_animation` calls on them
//!   are dropped, and the host's are replayed. A replay is allowed only for (class, animation) pairs our own
//!   game has started for that class (rule 4: network values going into a native call), unless the debug
//!   setting `enemy_replay_unverified` is on. Position and rotation follow the snapshots.
//! - **No local deaths.** Backing enemies get `EmBase.onNoDieSuperArmor` (HP clamps at 1, no local stagger).
//!   On `EntityDied` we clear it and call `EmBase.setDieRtn`, the scripts' own kill, so our quest scripts see
//!   a real death.
//! - **Damage.** The client's hits reach the host through the client's puppet on the host, which replays the
//!   same attacks and damages the host's enemies through the engine's own damage path (deaths, drops, scripts
//!   included). The client shows its own hits at once ([`HpView`]) until the host's HP catches up. Enemy
//!   attacks on the client come the same way: a backing enemy replays the host's attack animation here.

use std::collections::{HashMap, HashSet};
use std::ffi::{CStr, CString, c_void};
use std::ptr::{addr_of, addr_of_mut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex, RwLock};

use crossbeam_queue::ArrayQueue;

use crate::hooks::{self, game};
use crate::net::protocol::{self, Datagram, EntityState, Event, SpawnEntity};
use crate::sdk::{self, BehaviorAppBase, Entity, SpawnMatrix, SpawnParams, cVec4};
use crate::state::{self, EnemyStats, Q, Role};
use crate::sync::{interp, world};

/// Host snapshot rate.
const SEND_EVERY_MS: f64 = 50.0;
/// Host: an animation call identical to the last one sent for that enemy is re-sent at most this often
/// (some AI routines restart their idle every frame).
const ANIM_REPEAT_MS: f64 = 500.0;
/// Host: how often the entity list is scanned for networkable entities the spawn hook missed.
const SCAN_EVERY_MS: f64 = 250.0;
/// Host: while nothing is sent, how often the entity classes present are logged (at most `MAX_DIAG_LOGS`
/// diagnostic lines per session).
const DIAG_EVERY_MS: f64 = 10_000.0;
const MAX_DIAG_LOGS: u32 = 20;
/// Host: HP 0 without a die state counts as a death only after this long (a boss can touch 0 between phases).
const ZERO_HP_DEATH_MS: f64 = 1000.0;
/// Client: a host entity waits this long for our own game to spawn its match before we spawn a copy.
const PAIR_GRACE_MS: f64 = 2000.0;
/// Client: our own networkable entity waits this long for a host match before it is removed.
const LOCAL_GRACE_MS: f64 = 3000.0;
/// Client: pair with our own entity of the same model within this distance (m) of the host's.
const PAIR_DIST: f32 = 30.0;
/// Client: retries for a spawn whose assets may still be loading.
const SPAWN_ATTEMPTS: u32 = 5;
const SPAWN_RETRY_MS: f64 = 1000.0;
/// Client: at most this many copies spawned per frame, so a big wave does not hitch one frame.
const SPAWNS_PER_TICK: usize = 4;
/// Client: don't terminate the same entity again before this.
const TERMINATE_RETRY_MS: f64 = 2000.0;
/// Client: without a host snapshot for this long (host loading, stalled), our own AI runs our enemies again.
const STALE_MS: f64 = 1000.0;
/// Client: our own hits stay on the shown HP this long at most, waiting for the host's HP to reflect them.
const PENDING_HIT_MS: f64 = 1500.0;
/// Client: entity position error above which the copy snaps instead of blending (m).
const SNAP_DISTANCE: f32 = 5.0;
/// Size cap of the replay allowlist and of the refused-replay log.
const MAX_KNOWN: usize = 8192;
const MAX_REFUSED_LOGS: usize = 256;

// -------------------------------------------------------------------------------------------------------
// Shared with the hooks (which can run inside game calls we make, so no lock is held across a game call)

/// Client: set while we spawn an entity the host asked for.
static HOST_REQUESTED: AtomicBool = AtomicBool::new(false);
/// Host, spawn hook -> tick: networkable entities our game just spawned (`net_id` holds the handle).
static SPAWNED: LazyLock<ArrayQueue<SpawnEntity>> = LazyLock::new(|| ArrayQueue::new(1024));
/// Host, animation hook -> tick: (entity handle, `start_animation` arguments).
static ANIMS: LazyLock<ArrayQueue<(u32, [u32; 4])>> = LazyLock::new(|| ArrayQueue::new(4096));
/// Host: record enemy animation calls.
static RECORDING: AtomicBool = AtomicBool::new(false);
/// Client: behaviors (addresses) whose own AI may not start animations: the host's enemy decides.
static BLOCKED: LazyLock<RwLock<HashSet<usize>>> = LazyLock::new(|| RwLock::new(HashSet::new()));
/// (vtable, animation id) pairs our own game's enemies asked for, including blocked calls: the replay
/// allowlist. Kept for the whole run, so it grows as more enemy types and moves are seen.
static KNOWN: LazyLock<Mutex<HashSet<(usize, u32)>>> = LazyLock::new(|| Mutex::new(HashSet::new()));
/// Client: the behavior whose host call is being replayed (let through the block).
static APPLYING: AtomicUsize = AtomicUsize::new(0);

/// Reads what we need from a spawn call. `params` comes from the game.
fn read_spawn(params: *const SpawnParams, handle: u32) -> Option<SpawnEntity> {
    // SAFETY: the game passes a valid SpawnParams; the name and matrix pointers are null-checked.
    unsafe {
        let p = params.as_ref()?;
        if p.name.is_null() {
            return None;
        }
        let name = CStr::from_ptr(p.name.cast()).to_bytes();
        let name = String::from_utf8(name.get(..name.len().min(protocol::MAX_ENTITY_NAME))?.to_vec()).ok()?;
        let (pos, rot) = match p.matrix.as_ref() {
            Some(m) => (m.position, m.rotation),
            None => (cVec4::default(), cVec4::default()),
        };
        Some(SpawnEntity { net_id: handle, name, model: p.model, pos, rot })
    }
}

/// Entity spawn detour. May run on any game thread: it only reads the call and queues a record.
pub fn on_spawn(spawner: *mut c_void, params: *const SpawnParams) -> *mut Entity {
    let e = hooks::call_spawn(spawner, params);
    if e.is_null() || HOST_REQUESTED.load(Relaxed) || state::role() != Role::Host || !state::ENEMY_SYNC.load(Relaxed) {
        return e;
    }
    if world::behavior(e).is_some_and(|(_, c)| c.is_networkable && c.is_app_base)
        && let Some(s) = read_spawn(params, world::entity_handle(e))
    {
        let _ = SPAWNED.force_push(s);
    }
    e
}

/// `EmBase::start_animation` detour (every enemy class). May run on any game thread.
pub fn on_em_start_animation(this: *mut c_void, anim: u32, variant: u32, a3: u32, a4: u32) {
    let p = this as usize;
    // SAFETY: the engine calls this on a live EmBase, which starts with a BehaviorAppBase; the entity
    // back-pointer is null-checked before its handle is read.
    if let Some(b) = unsafe { (this as *const BehaviorAppBase).as_ref() }
        && APPLYING.load(Relaxed) != p
    {
        if let Ok(mut k) = KNOWN.lock()
            && k.len() < MAX_KNOWN
        {
            k.insert((b.vtable as usize, anim));
        }
        if BLOCKED.read().is_ok_and(|s| s.contains(&p)) {
            return; // client: the host's enemy decides what this one does
        }
        if RECORDING.load(Relaxed)
            && let Some(e) = unsafe { b.entity.as_ref() }
        {
            let _ = ANIMS.force_push((e.handle, [anim, variant, a3, a4]));
        }
    }
    hooks::call_em_start_animation(this, anim, variant, a3, a4);
}

fn set_blocked(s: HashSet<usize>) {
    if let Ok(mut b) = BLOCKED.write() {
        *b = s;
    }
}

fn unblock(b: *mut BehaviorAppBase) {
    if let Ok(mut s) = BLOCKED.write() {
        s.remove(&(b as usize));
    }
}

// -------------------------------------------------------------------------------------------------------
// Pure logic (unit-tested)

/// Client: the HP shown on a backing enemy. It is the host's value minus our own recent hits that have not
/// shown up there yet (our puppet on the host lands the same hits about one round trip later), so a hit
/// shows at once without flickering back. Never 0: the enemy dies only when the host says so.
#[derive(Debug, Default, Clone, Copy)]
struct HpView {
    written: Option<u32>,
    host: u32,
    pending: u32,
    since_ms: f64,
}

impl HpView {
    /// `host`: the host's HP; `local`: what the enemy has here now (our hits since last frame lowered it).
    fn step(&mut self, host: u32, local: u32, now: f64) -> u32 {
        if let Some(w) = self.written {
            let hit = w.saturating_sub(local);
            if hit > 0 {
                if self.pending == 0 {
                    self.since_ms = now;
                }
                self.pending = self.pending.saturating_add(hit);
            }
            // The host's HP dropped: most likely our hit landing there.
            self.pending = self.pending.saturating_sub(self.host.saturating_sub(host));
        }
        if self.pending > 0 && now - self.since_ms > PENDING_HIT_MS {
            self.pending = 0; // our puppet missed on the host: the host's value stands
        }
        self.host = host;
        let show = host.saturating_sub(self.pending).max(1);
        self.written = Some(show);
        show
    }
}

/// Client: which of our own candidates (positions of free entities of the right model) backs a host entity
/// at `at`: the nearest within `PAIR_DIST`, or the nearest at any distance when it is the only one of its
/// model on both sides (a boss can stand far from the host's after separate loads).
fn pick(at: cVec4, unique: bool, cands: &[cVec4]) -> Option<usize> {
    cands
        .iter()
        .enumerate()
        .filter(|(_, p)| unique || p.dist(at) <= PAIR_DIST)
        .min_by(|a, b| a.1.dist(at).total_cmp(&b.1.dist(at)))
        .map(|(i, _)| i)
}

// -------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backing {
    /// One of our own game's entities.
    Paired,
    /// A copy we spawned.
    Copy,
}

/// Host: one networked entity of ours.
struct Owned {
    handle: u32,
    dead: bool,
    zero_hp_since: Option<f64>,
    last_anim: Option<([u32; 4], f64)>,
}

/// Client: one host entity.
struct Mirror {
    spawn: SpawnEntity,
    /// When we last started waiting for a match (announcement, or our backing entity vanished).
    waiting_since_ms: f64,
    local: Option<(u32, Backing)>,
    target: Option<EntityState>,
    /// When `target` arrived (local clock).
    target_ms: f64,
    hp: HpView,
    dead: bool,
    spawn_attempts: u32,
    next_spawn_ms: f64,
}

impl Mirror {
    fn host_pos(&self) -> cVec4 {
        self.target.map_or(self.spawn.pos, |t| t.pos)
    }

    /// Still looking for one of our own entities: unbacked, or backed only by a copy.
    fn wants_pair(&self) -> bool {
        !self.dead && self.local.is_none_or(|(_, b)| b == Backing::Copy)
    }
}

/// One of our networkable entities this frame.
struct Local {
    handle: u32,
    b: *mut BehaviorAppBase,
    em: bool,
    model: u32,
    pos: cVec4,
    hp: u32,
}

fn locals() -> Vec<Local> {
    world::entities()
        .into_iter()
        .filter_map(|(handle, e)| {
            let (b, c) = world::behavior(e)?;
            if !(c.is_networkable && c.is_app_base) {
                return None;
            }
            let (pos, hp, model) = read_state(b);
            Some(Local { handle, b, em: c.is_em_base, model, pos, hp })
        })
        .collect()
}

/// Validated networkable behavior for `handle`, and whether it is an `EmBase`.
fn networkable(handle: u32) -> Option<(*mut BehaviorAppBase, bool)> {
    let (b, c) = world::behavior(world::entity_by_handle(handle)?)?;
    (c.is_networkable && c.is_app_base).then_some((b, c.is_em_base))
}

fn read_state(b: *mut BehaviorAppBase) -> (cVec4, u32, u32) {
    // SAFETY: validated BehaviorAppBase.
    unsafe { (addr_of!((*b).position).read(), addr_of!((*b).health).read(), addr_of!((*b).model_index).read()) }
}

#[derive(Default)]
pub struct EntitySync {
    role: Option<Role>,
    by_handle: HashMap<u32, u32>,
    // Host
    next_id: u32,
    owned: HashMap<u32, Owned>,
    last_send_ms: f64,
    seq: u32,
    last_scan_ms: f64,
    last_diag_ms: f64,
    diag_logs: u32,
    scan_logs: u32,
    // Client
    mirrors: HashMap<u32, Mirror>,
    first_seen: HashMap<u32, f64>,
    terminated: HashMap<u32, f64>,
    last_batch_ms: f64,
    refused_logged: HashSet<(usize, u32)>,
    stats: EnemyStats,
}

impl EntitySync {
    pub fn start(&mut self, role: Role) {
        self.end();
        *self = Self { role: Some(role), next_id: 1, ..Default::default() };
        self.stats.host = role == Role::Host;
        while SPAWNED.pop().is_some() {}
        while ANIMS.pop().is_some() {}
        if role == Role::Host {
            RECORDING.store(true, Relaxed);
            // Late join: announce everything already alive (AutomataMP `on_enter_server`).
            let n = self.announce_untracked();
            tracing::info!("enemy sync (host): announced {n} live entities");
        }
    }

    /// Host: registers every live networkable entity not yet sent. Run at session start and every
    /// `SCAN_EVERY_MS`, so entities spawned by a path the spawn hook doesn't see are still sent.
    fn announce_untracked(&mut self) -> usize {
        let mut n = 0;
        for l in locals() {
            if l.hp == 0 || self.by_handle.contains_key(&l.handle) {
                continue;
            }
            let name = world::entity_by_handle(l.handle).map(world::entity_name).unwrap_or_default();
            // SAFETY: validated BehaviorAppBase.
            let rot = unsafe { addr_of!((*l.b).rotation).read() };
            self.host_register(SpawnEntity { net_id: l.handle, name, model: l.model, pos: l.pos, rot });
            n += 1;
        }
        n
    }

    /// Host, while nothing is being sent: which classes the entity list holds, and whether we count them as
    /// networkable (an empty class chain means the RTTI lookup failed).
    fn log_entity_classes(&self) {
        let mut classes: Vec<(String, bool, usize, u32)> = Vec::new();
        for (_, e) in world::entities() {
            let (name, net, chain) = match world::behavior(e) {
                Some((_, c)) => (c.short_name(), c.is_networkable && c.is_app_base, c.chain.len()),
                None => ("<no behavior>".into(), false, 0),
            };
            match classes.iter_mut().find(|c| c.0 == name) {
                Some(c) => c.3 += 1,
                None => classes.push((name, net, chain, 1)),
            }
        }
        classes.sort_by(|a, b| b.3.cmp(&a.3));
        let list: Vec<String> = classes
            .iter()
            .take(40)
            .map(|(n, net, chain, count)| format!("{n} x{count}{}{}", if *net { " [networked]" } else { "" }, if *chain == 0 { " [no RTTI]" } else { "" }))
            .collect();
        tracing::warn!("enemy sync (host): sending no enemies; {} entities here: {}", world::entities().len(), list.join(", "));
    }

    /// Session over: our enemies go back to their own AI and can die again.
    pub fn end(&mut self) {
        RECORDING.store(false, Relaxed);
        set_blocked(HashSet::new());
        for m in self.mirrors.values() {
            if let (Some((h, _)), false) = (m.local, m.dead)
                && let Some((b, true)) = networkable(h)
            {
                world::set_no_die(b, false);
            }
        }
        *self = Self::default();
    }

    pub fn stats(&self) -> EnemyStats {
        self.stats.clone()
    }

    pub fn net_id_of(&self, handle: u32) -> Option<(u32, &'static str)> {
        let id = *self.by_handle.get(&handle)?;
        let kind = match self.mirrors.get(&id).and_then(|m| m.local) {
            Some((_, Backing::Paired)) => "paired",
            Some((_, Backing::Copy)) => "copy",
            None => "host",
        };
        Some((id, kind))
    }

    /// Gives a new entity a net id and announces it (`s.net_id` holds its handle on entry).
    fn host_register(&mut self, mut s: SpawnEntity) {
        let handle = s.net_id;
        if self.by_handle.contains_key(&handle) {
            return;
        }
        if s.name.is_empty() || !s.name.bytes().all(|b| b.is_ascii_graphic()) {
            // Many enemies have an empty entity name (seen in-game 2026-11-22: model 0x20052). The spawn
            // function only uses the name as a label (AutomataMP spawns enemies as "FreeEnemy"); the model
            // decides what spawns. The client's `decode_event` rejects empty names, so send one.
            s.name = fallback_name(s.model);
        }
        let id = self.next_id;
        self.next_id += 1;
        s.net_id = id;
        self.owned.insert(id, Owned { handle, dead: false, zero_hp_since: None, last_anim: None });
        self.by_handle.insert(handle, id);
        state::send_event(Event::SpawnEntity(s));
    }

    pub fn on_event(&mut self, e: Event, now: f64) {
        if self.role != Some(Role::Client) {
            return;
        }
        match e {
            Event::SpawnEntity(s) => {
                if !self.mirrors.contains_key(&s.net_id) {
                    let id = s.net_id;
                    let m = Mirror {
                        spawn: s,
                        waiting_since_ms: now,
                        local: None,
                        target: None,
                        target_ms: 0.0,
                        hp: HpView::default(),
                        dead: false,
                        spawn_attempts: 0,
                        next_spawn_ms: 0.0,
                    };
                    self.mirrors.insert(id, m);
                }
            }
            Event::EntityAnim { net_id, args } => {
                let Some(m) = self.mirrors.get(&net_id).filter(|m| !m.dead) else { return };
                if let Some((h, _)) = m.local
                    && let Some((b, true)) = networkable(h)
                {
                    self.replay(b, args);
                }
            }
            Event::EntityDied { net_id } => {
                let Some(m) = self.mirrors.get_mut(&net_id).filter(|m| !m.dead) else { return };
                m.dead = true;
                if let Some((h, _)) = m.local {
                    kill(h);
                    self.stats.kills += 1;
                }
            }
            Event::DestroyEntity { net_id } => {
                let Some(m) = self.mirrors.remove(&net_id) else { return };
                if let Some((h, _)) = m.local {
                    self.by_handle.remove(&h);
                    if !m.dead
                        && let Some((b, _)) = networkable(h)
                    {
                        unblock(b);
                        world::terminate(b);
                    }
                    // A dead one is left to finish dying; the engine removes it.
                }
            }
            _ => {}
        }
    }

    /// Client: replays one host animation call on our backing enemy.
    fn replay(&mut self, b: *mut BehaviorAppBase, args: [u32; 4]) {
        if sdk::EM_DIE_STATES.contains(&args[0]) {
            return; // deaths come as `EntityDied`
        }
        // SAFETY: validated BehaviorAppBase.
        let vt = unsafe { addr_of!((*b).vtable).read() } as usize;
        let known = KNOWN.lock().is_ok_and(|k| k.contains(&(vt, args[0])));
        if !known && !self.stats.replay_unverified {
            self.stats.refused += 1;
            if self.refused_logged.len() < MAX_REFUSED_LOGS && self.refused_logged.insert((vt, args[0])) {
                tracing::info!(
                    "enemy sync: host animation {:#x} not yet seen on this enemy class here (vtable {:#x}); not replayed",
                    args[0],
                    vt.wrapping_sub(game().addr.base)
                );
            }
            return;
        }
        let [a, v, a3, a4] = args;
        APPLYING.store(b as usize, Relaxed);
        hooks::call_em_start_animation(b.cast(), a, v, a3, a4);
        APPLYING.store(0, Relaxed);
        self.stats.replayed += 1;
    }

    pub fn on_batch(&mut self, at_ms: f64, items: Vec<EntityState>) {
        if self.role != Some(Role::Client) {
            return;
        }
        self.last_batch_ms = at_ms;
        for s in items {
            if let Some(m) = self.mirrors.get_mut(&s.net_id) {
                m.target = Some(s);
                m.target_ms = at_ms;
            }
        }
    }

    pub fn tick(&mut self, now: f64, dt: f32) {
        match self.role {
            Some(Role::Host) => self.tick_host(now),
            Some(Role::Client) => self.tick_client(now, dt),
            _ => {}
        }
    }

    fn tick_host(&mut self, now: f64) {
        while let Some(s) = SPAWNED.pop() {
            self.host_register(s);
        }
        if now - self.last_scan_ms >= SCAN_EVERY_MS {
            self.last_scan_ms = now;
            let n = self.announce_untracked();
            if n > 0 && self.scan_logs < MAX_DIAG_LOGS {
                self.scan_logs += 1;
                tracing::info!("enemy sync (host): found {n} entities the spawn hook did not report");
            }
            if self.owned.is_empty() && now - self.last_diag_ms >= DIAG_EVERY_MS && self.diag_logs < MAX_DIAG_LOGS {
                self.last_diag_ms = now;
                self.diag_logs += 1;
                self.log_entity_classes();
            }
        }
        while let Some((handle, args)) = ANIMS.pop() {
            let Some(&id) = self.by_handle.get(&handle) else { continue };
            let Some(o) = self.owned.get_mut(&id).filter(|o| !o.dead) else { continue };
            if sdk::EM_DIE_STATES.contains(&args[0]) {
                o.dead = true;
                state::send_event(Event::EntityDied { net_id: id });
            } else if args[0] < protocol::MAX_ANIM
                && o.last_anim.is_none_or(|(last, t)| last != args || now - t >= ANIM_REPEAT_MS)
            {
                o.last_anim = Some((args, now));
                state::send_event(Event::EntityAnim { net_id: id, args });
            }
        }
        if now - self.last_send_ms < SEND_EVERY_MS {
            return;
        }
        self.last_send_ms = now;
        let mut items = Vec::with_capacity(self.owned.len());
        let mut gone = Vec::new();
        for (&id, o) in &mut self.owned {
            let Some((b, _)) = networkable(o.handle) else {
                gone.push(id);
                continue;
            };
            let (pos, hp, _) = read_state(b);
            // SAFETY: validated BehaviorAppBase.
            let rot = unsafe { addr_of!((*b).rotation).read() };
            if hp > 0 {
                o.zero_hp_since = None;
            } else if !o.dead && now - *o.zero_hp_since.get_or_insert(now) >= ZERO_HP_DEATH_MS {
                o.dead = true;
                state::send_event(Event::EntityDied { net_id: id });
            }
            if o.dead {
                continue;
            }
            // Only yaw is sent (as AutomataMP and nier_coop do): the other rotation components and position.w
            // are not ours to vouch for, and one non-finite value would make the client drop the whole batch.
            let pos = cVec4::new(pos.x, pos.y, pos.z, 1.0);
            let rot = cVec4::new(0.0, rot.y, 0.0, 0.0);
            if pos.is_sane() && rot.is_sane() {
                items.push(EntityState { net_id: id, pos, rot, hp: hp.min(protocol::MAX_HP) });
            } else if self.diag_logs < MAX_DIAG_LOGS {
                self.diag_logs += 1;
                tracing::warn!("enemy sync (host): not sending entity {id}: position {pos:?} / yaw {} out of range", rot.y);
            }
        }
        for id in gone {
            if let Some(o) = self.owned.remove(&id) {
                self.by_handle.remove(&o.handle);
            }
            state::send_event(Event::DestroyEntity { net_id: id });
        }
        // An empty batch still goes out: it tells the client the host is alive and has nothing to update.
        let chunks: Vec<&[EntityState]> = if items.is_empty() { vec![&[]] } else { items.chunks(protocol::ENTITIES_PER_DATAGRAM).collect() };
        for chunk in chunks {
            self.seq = self.seq.wrapping_add(1);
            let d = Datagram::Entities { seq: self.seq, time_ms: now as u32, items: chunk.to_vec() };
            let _ = Q.out_datagrams.force_push(d);
        }
        self.stats.tracked = self.owned.values().filter(|o| !o.dead).count();
    }

    fn tick_client(&mut self, now: f64, dt: f32) {
        let locals = locals();
        let present: HashMap<u32, usize> = locals.iter().enumerate().map(|(i, l)| (l.handle, i)).collect();

        // Backing entities our game removed (area unload, its own cleanup): look for a new one.
        for m in self.mirrors.values_mut() {
            if let Some((h, _)) = m.local
                && !present.contains_key(&h)
            {
                self.by_handle.remove(&h);
                m.local = None;
                m.waiting_since_ms = now;
            }
        }

        // Our own entities that back nothing yet. Corpses are never paired or removed: they finish dying.
        self.terminated.retain(|_, t| now - *t < TERMINATE_RETRY_MS);
        self.first_seen.retain(|h, _| present.contains_key(h));
        let mut free: Vec<usize> = (0..locals.len())
            .filter(|&i| {
                let l = &locals[i];
                l.hp > 0 && !self.by_handle.contains_key(&l.handle) && !self.terminated.contains_key(&l.handle)
            })
            .collect();
        for &i in &free {
            self.first_seen.entry(locals[i].handle).or_insert(now);
        }

        // Pair host entities with our own (also replacing a copy we spawned earlier).
        let wanting: Vec<u32> = self.mirrors.iter().filter(|(_, m)| m.wants_pair()).map(|(&id, _)| id).collect();
        for id in wanting {
            let (model, at) = {
                let m = &self.mirrors[&id];
                (m.spawn.model, m.host_pos())
            };
            let same: Vec<usize> = free.iter().copied().filter(|&i| locals[i].model == model).collect();
            if same.is_empty() {
                continue;
            }
            let host_same = self.mirrors.values().filter(|m| m.wants_pair() && m.spawn.model == model).count();
            let cands: Vec<cVec4> = same.iter().map(|&i| locals[i].pos).collect();
            let Some(k) = pick(at, host_same == 1 && same.len() == 1, &cands) else { continue };
            let l = &locals[same[k]];
            free.retain(|&i| i != same[k]);
            let m = self.mirrors.get_mut(&id).expect("id from the map");
            if let Some((old, Backing::Copy)) = m.local {
                self.by_handle.remove(&old);
                if let Some((b, _)) = networkable(old) {
                    unblock(b);
                    world::terminate(b);
                }
                tracing::info!("enemy sync: replaced the copy of {} with our own", m.spawn.name);
            }
            m.local = Some((l.handle, Backing::Paired));
            m.hp = HpView::default();
            self.by_handle.insert(l.handle, id);
            self.first_seen.remove(&l.handle);
            if l.em {
                world::set_no_die(l.b, true);
            }
        }

        // Spawn copies of host entities nothing of ours matched.
        let mut spawned = 0;
        for (&id, m) in &mut self.mirrors {
            if m.dead || m.local.is_some() || m.spawn_attempts >= SPAWN_ATTEMPTS || spawned >= SPAWNS_PER_TICK {
                continue;
            }
            if now - m.waiting_since_ms < PAIR_GRACE_MS || now < m.next_spawn_ms {
                continue;
            }
            spawned += 1;
            let rot = m.target.map_or(m.spawn.rot, |t| t.rot);
            match spawn_copy(&m.spawn, m.host_pos(), rot) {
                Some((h, b, em)) => {
                    m.local = Some((h, Backing::Copy));
                    m.hp = HpView::default();
                    self.by_handle.insert(h, id);
                    if em {
                        world::set_no_die(b, true);
                    }
                }
                None => {
                    m.spawn_attempts += 1;
                    m.next_spawn_ms = now + SPAWN_RETRY_MS;
                    if m.spawn_attempts == SPAWN_ATTEMPTS {
                        tracing::warn!("enemy sync: could not spawn {} (model {:#x})", m.spawn.name, m.spawn.model);
                    }
                }
            }
        }

        // Remove our own entities the host has no match for.
        for &i in &free {
            let l = &locals[i];
            if self.first_seen.get(&l.handle).is_some_and(|&t| now - t >= LOCAL_GRACE_MS) {
                world::terminate(l.b);
                self.terminated.insert(l.handle, now);
                self.stats.removed += 1;
            }
        }

        // Drive the backing entities from the host's snapshots.
        let fresh = now - self.last_batch_ms < STALE_MS;
        let mut blocked = HashSet::new();
        let (mut paired, mut copies, mut waiting, mut no_updates) = (0, 0, 0, 0);
        for m in self.mirrors.values_mut().filter(|m| !m.dead) {
            let Some((h, kind)) = m.local else {
                waiting += 1;
                continue;
            };
            match kind {
                Backing::Paired => paired += 1,
                Backing::Copy => copies += 1,
            }
            // An enemy the host stopped sending (or never sent) runs on our own AI until its updates resume.
            let t = m.target.filter(|_| fresh && now - m.target_ms < STALE_MS);
            if t.is_none() && fresh {
                no_updates += 1;
            }
            let (Some(t), Some((b, em))) = (t, networkable(h)) else { continue };
            if em {
                blocked.insert(b as usize);
            }
            // SAFETY: validated BehaviorAppBase.
            unsafe {
                let hp = addr_of_mut!((*b).health);
                hp.write(m.hp.step(t.hp, hp.read(), now));
                let pos = addr_of_mut!((*b).position);
                let cur = pos.read();
                let p = interp::correct(cur, t.pos, dt, SNAP_DISTANCE);
                pos.write(cVec4 { w: cur.w, ..p });
                addr_of_mut!((*b).rotation.y).write(t.rot.y); // yaw only, as the host sends it
            }
        }
        set_blocked(blocked);
        self.stats.tracked = paired + copies + waiting;
        self.stats.paired = paired;
        self.stats.copies = copies;
        self.stats.waiting = waiting;
        self.stats.no_updates = no_updates;
        self.stats.host_stale = !fresh && !self.mirrors.is_empty();
    }

    /// Client debug setting: replay host animations this game has not seen for that enemy class.
    pub fn set_replay_unverified(&mut self, on: bool) {
        self.stats.replay_unverified = on;
    }
}

/// Name sent for an entity whose own name is empty or not plain ASCII: "em" plus the model's low 16 bits
/// (0x20052 -> "em0052"), matching the game's own enemy naming.
fn fallback_name(model: u32) -> String {
    format!("em{:04x}", model & 0xFFFF)
}

/// Client: the host's enemy died; ours dies the way the scripts kill an enemy, so our quest scripts follow.
fn kill(h: u32) {
    let Some((b, em)) = networkable(h) else { return };
    unblock(b);
    if !em {
        world::terminate(b);
        return;
    }
    APPLYING.store(b as usize, Relaxed);
    world::set_no_die(b, false);
    // SAFETY: validated BehaviorAppBase.
    unsafe { addr_of_mut!((*b).health).write(0) };
    world::die_routine(b);
    APPLYING.store(0, Relaxed);
}

/// Client: spawns a copy of a host entity through the game's own spawn function.
fn spawn_copy(s: &SpawnEntity, pos: cVec4, rot: cVec4) -> Option<(u32, *mut BehaviorAppBase, bool)> {
    let name = CString::new(s.name.as_bytes()).ok()?;
    let m = SpawnMatrix::at(pos, rot);
    let params = SpawnParams::new(&name, s.model, &m);
    let g = game();
    HOST_REQUESTED.store(true, Relaxed);
    let e = hooks::call_spawn(g.addr.abs(g.addr.spawner) as *mut c_void, &params);
    HOST_REQUESTED.store(false, Relaxed);
    if e.is_null() {
        return None;
    }
    let h = world::entity_handle(e);
    let (b, em) = networkable(h)?;
    world::set_suspend(b.cast(), false);
    Some((h, b, em))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hp_shows_our_hits_until_the_host_catches_up() {
        let mut v = HpView::default();
        assert_eq!(v.step(100, 100, 0.0), 100); // first sight: the host's value
        assert_eq!(v.step(100, 80, 16.0), 80); // our hit shows at once
        assert_eq!(v.step(100, 80, 32.0), 80); // the host has not seen it yet: no flicker back
        assert_eq!(v.step(80, 80, 150.0), 80); // it landed there too
        assert_eq!(v.step(60, 80, 166.0), 60); // the host's own hits come through
        assert_eq!(v.pending, 0);
    }

    #[test]
    fn hp_drops_a_hit_the_host_never_saw() {
        let mut v = HpView::default();
        v.step(100, 100, 0.0);
        assert_eq!(v.step(100, 70, 10.0), 70);
        assert_eq!(v.step(100, 70, 1000.0), 70);
        assert_eq!(v.step(100, 70, 10.0 + PENDING_HIT_MS + 1.0), 100); // our puppet missed there: host wins
    }

    #[test]
    fn hp_never_shows_zero() {
        let mut v = HpView::default();
        v.step(10, 10, 0.0);
        assert_eq!(v.step(10, 0, 10.0), 1); // a local "kill" waits for the host's verdict
        assert_eq!(v.step(0, 1, 20.0), 1);
    }

    #[test]
    fn unnamed_entities_get_a_name_the_client_accepts() {
        assert_eq!(fallback_name(0x20052), "em0052");
        let e = Event::SpawnEntity(SpawnEntity { net_id: 1, name: fallback_name(0x21020), model: 0x21020, pos: cVec4::default(), rot: cVec4::default() });
        assert!(protocol::decode_event(&protocol::encode(&e)).is_ok());
    }

    #[test]
    fn pairs_nearest_in_range_or_a_unique_one_anywhere() {
        let at = cVec4::new(0.0, 0.0, 0.0, 1.0);
        let near = cVec4::new(10.0, 0.0, 0.0, 1.0);
        let nearer = cVec4::new(5.0, 0.0, 0.0, 1.0);
        let far = cVec4::new(500.0, 0.0, 0.0, 1.0);
        assert_eq!(pick(at, false, &[near, nearer, far]), Some(1));
        assert_eq!(pick(at, false, &[far]), None);
        assert_eq!(pick(at, true, &[far]), Some(0)); // a boss after separate loads
        assert_eq!(pick(at, false, &[]), None);
    }
}
