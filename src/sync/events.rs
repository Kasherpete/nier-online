//! Shared death and cutscene sync, both directions (either player can trigger them).
//!
//! **Shared death.** When the local character dies by itself, send `PlayerDied`; when the peer's does, kill the
//! local character the way a kill area does (`life = health = 0`, then the death state), so the game plays its
//! own death and reloads the last checkpoint. Port of nier_coop `hooks/mod.rs` (`kill_local`, the follow-death
//! logic), made event-driven instead of watching the peer's HP. A death caused by `PlayerDied` is not sent back.
//! After a client death, both games reload their *own* last checkpoint, which can differ: once both are back,
//! the client warps to the host if they are more than `REJOIN_DIST` apart.
//!
//! **Cutscenes.** Every scene action changes state through `Action::request_state` (hooked). When a cutscene
//! (`PhaseEventAction`) starts playing here, its layout id is sent with our position. The peer warps its
//! character there, then plays its own action with that id through the `PhaseEventAction.play` native (which
//! loads it first if needed). Port of the cutscene part of nier_coop `hooks/events.rs`: one cutscene at a time,
//! skipped if it already started (or finished) on the receiving side, dropped after `PLAY_TIMEOUT_MS`.

use std::ptr::{addr_of, addr_of_mut};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

use crossbeam_queue::ArrayQueue;

use crate::hooks::{self, game};
use crate::net::protocol::{Event, PlayerSnapshot};
use crate::sdk::{self, ActionScene, Pl0000, SceneAction, StateReq, cVec4, phase};
use crate::state::{self, Role};
use crate::sync::world;

/// How long a peer's cutscene waits to be played here (our own cutscene to end, the warp's area to load).
const PLAY_TIMEOUT_MS: f64 = 60_000.0;
/// A cutscene we started because the peer did is not sent back within this time.
const ECHO_MS: u32 = 120_000;
/// Closer than this to the peer's cutscene position: no warp needed.
const WARP_MIN_DIST: f32 = 5.0;
/// After a client death, farther than this from the respawned host: the two reloaded different checkpoints.
const REJOIN_DIST: f32 = 30.0;
/// How long a death that arrived while our character was away (loading, hacking, flight unit) is kept.
const AWAY_DEATH_MS: f64 = 60_000.0;
/// How long the client waits for the host to respawn before giving up on rejoining it.
const REJOIN_MS: f64 = 60_000.0;
/// Bounds for the action scene walk; far above what the game loads.
const MAX_GROUPS: usize = 0x10000;
const MAX_ACTIONS: usize = 0x10000;

// Shared with the request_state hook (no locks: it runs inside game calls, including our own `play`).
/// Cutscene sync is on for this session.
static CUTSCENES_ON: AtomicBool = AtomicBool::new(false);
/// Hook -> tick: ids of cutscenes the local game started playing.
static STARTED: LazyLock<ArrayQueue<u32>> = LazyLock::new(|| ArrayQueue::new(64));
/// Cutscenes started here because the peer did: `id << 32 | session ms`, so their PLAYING is not sent back.
static FROM_PEER: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
static FROM_PEER_NEXT: AtomicUsize = AtomicUsize::new(0);

fn now_u32() -> u32 {
    state::now_ms() as u32
}

fn mark_from_peer(id: u32) {
    let i = FROM_PEER_NEXT.fetch_add(1, Relaxed) % FROM_PEER.len();
    FROM_PEER[i].store((id as u64) << 32 | now_u32() as u64, Relaxed);
}

fn is_from_peer(id: u32) -> bool {
    let now = now_u32();
    FROM_PEER.iter().any(|s| {
        let v = s.load(Relaxed);
        v != 0 && (v >> 32) as u32 == id && now.wrapping_sub(v as u32) < ECHO_MS
    })
}

/// `Action::request_state` detour.
pub fn on_request_state(this: *mut SceneAction, req: *const StateReq) -> usize {
    let g = game();
    // SAFETY: the engine passes its own action and request; both are null-checked, and only the action's
    // documented prefix is read.
    let before = unsafe { this.as_ref() }.map(|a| (a.vtable, a.id, a.state));
    let hash = unsafe { req.as_ref() }.map(|q| q.hash);
    let r = hooks::call_request_state(this, req);
    if let (Some((vt, id, old)), Some(hash)) = (before, hash) {
        let cutscene = vt == g.addr.abs(g.addr.phase_vtable);
        if cutscene && hash == phase::PLAYING && old != phase::PLAYING && CUTSCENES_ON.load(Relaxed) && !is_from_peer(id) {
            let _ = STARTED.push(id);
        }
    }
    r
}

/// First loaded action matching `f`.
fn find_action(f: impl Fn(&SceneAction) -> bool) -> Option<*mut SceneAction> {
    let g = game();
    // SAFETY: a static object inside the module; the arrays it points to are null- and count-checked, and
    // only the actions' documented prefix is read.
    unsafe {
        let s = (g.addr.abs(g.addr.action_scene) as *const ActionScene).as_ref()?;
        if s.groups.is_null() || s.count > MAX_GROUPS {
            return None;
        }
        for i in 0..s.count {
            let Some(grp) = s.groups.add(i).read().as_ref() else { continue };
            if grp.actions.is_null() || grp.count > MAX_ACTIONS {
                continue;
            }
            for j in 0..grp.count {
                let p = grp.actions.add(j).read();
                if p.as_ref().is_some_and(&f) {
                    return Some(p);
                }
            }
        }
    }
    None
}

fn phase_vt() -> usize {
    let g = game();
    g.addr.abs(g.addr.phase_vtable)
}

/// Our cutscene with this layout id.
fn find_cutscene(id: u32) -> Option<*mut SceneAction> {
    let vt = phase_vt();
    find_action(|a| a.id == id && a.vtable == vt)
}

/// A cutscene is being started or played here right now.
fn cutscene_busy() -> bool {
    let vt = phase_vt();
    find_action(|a| a.vtable == vt && [phase::PLAY_REQ, phase::PLAYING, phase::PLAY_END].contains(&a.state)).is_some()
}

fn is_alive(pl: *mut Pl0000) -> bool {
    // SAFETY: validated Pl0000.
    unsafe { addr_of!((*pl).base.health).read() > 0 && addr_of!((*pl).life).read() > 0 }
}

fn position(pl: *mut Pl0000) -> (cVec4, cVec4) {
    // SAFETY: validated Pl0000.
    unsafe { (addr_of!((*pl).base.position).read(), addr_of!((*pl).base.rotation).read()) }
}

/// Kills the local character the way a kill area does, so the game plays the death and reloads the last
/// checkpoint (nier_coop `kill_local`). Bypasses our `start_animation` detour, so it is not sent as an `Anim`.
fn kill_local(pl: *mut Pl0000) {
    if !is_alive(pl) {
        return; // dying twice would restart the death
    }
    // SAFETY: validated Pl0000.
    unsafe {
        addr_of_mut!((*pl).life).write(0);
        addr_of_mut!((*pl).base.health).write(0);
    }
    hooks::call_start_animation(pl.cast(), sdk::ANIM_DEATH, 0, 0, 1);
}

/// A peer cutscene waiting to be played here.
struct PendingPlay {
    id: u32,
    pos: cVec4,
    rot: cVec4,
    at_ms: f64,
    warped: bool,
    logged_missing: bool,
}

#[derive(Default)]
pub struct Events {
    active: bool,
    role: Option<Role>,
    shared_death: bool,
    /// Our character was present and alive last frame.
    local_alive: bool,
    /// Our current death was caused by the peer's: not sent back.
    killed_by_peer: bool,
    /// The peer died at this time and we have not followed yet (our character was away).
    peer_died_at: Option<f64>,
    /// Our character was away (loading, hacking, flight unit) while `peer_died_at` waited.
    away_at_peer_death: bool,
    /// Client: we died at this time; once both are back, warp to the host if far apart.
    rejoin_since: Option<f64>,
    /// The peer's latest snapshot.
    peer: Option<PlayerSnapshot>,
    last_local: (cVec4, cVec4),
    plays: Vec<PendingPlay>,
}

impl Events {
    pub fn start(&mut self, role: Role) {
        let c = crate::config::get();
        *self = Self { active: true, role: Some(role), shared_death: c.shared_death, local_alive: self.local_alive, ..Default::default() };
        CUTSCENES_ON.store(c.cutscene_sync, Relaxed);
        while STARTED.pop().is_some() {}
        for s in &FROM_PEER {
            s.store(0, Relaxed);
        }
    }

    pub fn end(&mut self) {
        *self = Self { local_alive: self.local_alive, ..Default::default() };
        CUTSCENES_ON.store(false, Relaxed);
    }

    pub fn on_peer_snapshot(&mut self, s: PlayerSnapshot) {
        self.peer = Some(s);
    }

    /// The peer's character is in the world and alive.
    fn peer_alive(&self) -> Option<PlayerSnapshot> {
        self.peer.filter(|p| p.flags & PlayerSnapshot::FLAG_PL0000 != 0 && p.hp > 0)
    }

    pub fn on_event(&mut self, e: Event, now: f64) {
        if !self.active {
            return;
        }
        match e {
            Event::PlayerDied if self.shared_death => {
                tracing::info!("partner died");
                self.peer_died_at = Some(now);
                self.away_at_peer_death = false;
            }
            Event::Cutscene { id, pos, rot } if CUTSCENES_ON.load(Relaxed) => {
                tracing::info!("partner started cutscene {id:08x} at ({:.1}, {:.1}, {:.1})", pos.x, pos.y, pos.z);
                if !self.plays.iter().any(|p| p.id == id) {
                    self.plays.push(PendingPlay { id, pos, rot, at_ms: now, warped: false, logged_missing: false });
                }
            }
            _ => {}
        }
    }

    /// Once per frame, after the player sync.
    pub fn tick(&mut self, now: f64) {
        let local = world::player().map(|(_, p)| p);
        let alive = local.is_some_and(is_alive);
        if let Some(pl) = local {
            self.last_local = position(pl);
        }
        if alive != self.local_alive {
            self.local_alive = alive;
            if alive {
                self.killed_by_peer = false;
            } else if local.is_some() {
                // Health reached 0 with the character still here: a death, not an unload.
                if self.active && self.shared_death && !self.killed_by_peer {
                    tracing::info!("local player died: the partner dies too");
                    state::send_event(Event::PlayerDied);
                }
                if self.active && self.role == Some(Role::Client) {
                    self.rejoin_since = Some(now);
                }
                self.peer_died_at = None; // we reload anyway
            }
        }
        if !self.active {
            return;
        }
        self.follow_death(now, local, alive);
        self.send_started();
        self.play_pending(now, local, alive);
    }

    fn follow_death(&mut self, now: f64, local: Option<*mut Pl0000>, alive: bool) {
        if let Some(at) = self.peer_died_at {
            match local {
                _ if now - at > AWAY_DEATH_MS => {
                    tracing::warn!("partner's death was not followed within {} s; dropped", AWAY_DEATH_MS / 1000.0);
                    self.peer_died_at = None;
                    self.away_at_peer_death = false;
                }
                Some(pl) if alive => {
                    self.peer_died_at = None;
                    match self.peer_alive() {
                        // We were away (loading, hacking) and the peer has respawned meanwhile: dying now would
                        // make it reload a second time, so join it where it is instead.
                        Some(p) if std::mem::take(&mut self.away_at_peer_death) => {
                            tracing::info!("partner died while you were away and has respawned: warping to it");
                            world::warp(pl, p.pos, p.rot);
                        }
                        _ => {
                            tracing::info!("partner died: killing the local player to reload the last checkpoint");
                            state::push_chat("[co-op] Your partner died.".into());
                            self.killed_by_peer = true;
                            kill_local(pl);
                        }
                    }
                }
                Some(_) => self.peer_died_at = None, // already dead
                None => self.away_at_peer_death = true, // away: follow once back
            }
        }
        if let Some(since) = self.rejoin_since {
            if now - since > REJOIN_MS {
                self.rejoin_since = None;
            } else if let (Some(pl), true, Some(host)) = (local, alive, self.peer_alive()) {
                self.rejoin_since = None;
                let d = position(pl).0.dist(host.pos);
                if d > REJOIN_DIST {
                    tracing::info!("respawned {d:.0} m from the host (different checkpoint): warping to the host");
                    world::warp(pl, host.pos, host.rot);
                }
            }
        }
    }

    /// Sends the cutscenes our game started, with where our character stands.
    fn send_started(&mut self) {
        while let Some(id) = STARTED.pop() {
            let (pos, rot) = self.last_local;
            tracing::info!("cutscene {id:08x} started here: the partner joins");
            state::send_event(Event::Cutscene { id, pos, rot });
        }
    }

    /// Plays the peer's cutscenes here, one at a time: warp to the peer first (that also streams its area in),
    /// then start our own action with the same id once it is loaded and nothing else plays here.
    fn play_pending(&mut self, now: f64, local: Option<*mut Pl0000>, alive: bool) {
        let Some(p) = self.plays.first_mut() else { return };
        if now - p.at_ms > PLAY_TIMEOUT_MS {
            tracing::warn!("cutscene {:08x} dropped: not playable here within {} s", p.id, PLAY_TIMEOUT_MS / 1000.0);
            self.plays.remove(0);
            return;
        }
        let act = find_cutscene(p.id);
        // SAFETY: an action found in the live scene; only its documented prefix is read.
        let state = act.map(|a| unsafe { addr_of!((*a).state).read() });
        if let Some(s) = state.filter(|s| [phase::PLAY_REQ, phase::PLAYING, phase::PLAY_END, phase::DONE].contains(s)) {
            // Started here too (both triggered it), or already watched: playing it again left the player stuck
            // in event mode (nier_coop).
            tracing::info!("cutscene {:08x} already started here (state {s:08x})", p.id);
            self.plays.remove(0);
            return;
        }
        if cutscene_busy() {
            return; // ours first; starting a second one at once locks the player in event mode
        }
        let (Some(pl), true) = (local, alive) else { return };
        if !p.warped {
            p.warped = true;
            let d = position(pl).0.dist(p.pos);
            if d > WARP_MIN_DIST {
                tracing::info!("warping {d:.0} m to the partner for cutscene {:08x}", p.id);
                state::push_chat("[co-op] Your partner started a cutscene: teleporting to them.".into());
                world::warp(pl, p.pos, p.rot);
                return; // play from the next frame, once the warp has settled
            }
        }
        let Some(act) = act else {
            if !p.logged_missing {
                p.logged_missing = true;
                tracing::info!("cutscene {:08x} is not loaded here yet; waiting", p.id);
            }
            return;
        };
        let id = p.id;
        self.plays.remove(0);
        mark_from_peer(id);
        let g = game();
        // SAFETY: the `PhaseEventAction.play` native takes the action; `act` is a live PhaseEventAction.
        let ok = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(*mut SceneAction) -> u8>(g.addr.abs(g.addr.phase_play))(act) };
        tracing::info!("playing the partner's cutscene {id:08x} (state {:08x}): ok {ok}", state.unwrap_or(0));
    }
}
