//! Cutscene sync, both ways. Whoever's game starts a cutscene first (its player reached the trigger), the other
//! game plays the same one, and a later trigger of that cutscene on either game is refused for the rest of the
//! session, so nobody sees it twice.
//!
//! Two kinds, matched across the two games by layout id:
//! - `PhaseEventAction`: full cutscenes, started with the game's own `PhaseEventAction.play` native (which loads
//!   it first if needed).
//! - `CameraAction`: scripted camera events ("mini cutscenes", such as an enemy wave's intro), started by
//!   requesting their PLAY state the way the engine does.
//!
//! Ported from nier_coop `src/hooks/events.rs` (GPL-3.0), cutscene part only: every scene action changes state
//! through `Action::request_state` (hooked). nier_coop only went host -> client and only for `PhaseEventAction`.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};

use crate::hooks::{self, game};
use crate::net::protocol::Event;
use crate::sdk::{ActionScene, SceneAction, StateReq, camera, phase};
use crate::state;
use crate::sync::world;

/// How long a peer's cutscene waits to load here (each game loads action groups on its own).
const PLAY_TIMEOUT_MS: f64 = 60_000.0;
/// How often waiting cutscenes are looked for (each look walks every loaded action).
const RETRY_EVERY_MS: f64 = 250.0;
/// Bounds for walking the action scene (game memory).
const MAX_GROUPS: usize = 0x10000;
const MAX_ACTIONS: usize = 0x10000;

/// Scene-action state changes logged per session (finding which action a cutscene is; nier_coop logged these too).
static LOGGED: AtomicU32 = AtomicU32::new(0);
const MAX_LOGGED: u32 = 5000;
/// Classes whose state changes are only areas and spawners loading (9 in 10 lines of a 2026-11-22 session log):
/// not logged.
const NOISY: [&str; 8] = [
    "EntityLayoutAction",
    "PathCameraArea",
    "ShootingEnemyCurveAction",
    "EnemySetArea",
    "RecoveryPointArea",
    "AreaAction",
    "EnemyGenerator",
    "EntityLayoutArea",
];
/// A session is up (set by `sync::tick`).
static ACTIVE: AtomicBool = AtomicBool::new(false);
/// A cutscene just ended here (taken by `sync::player`).
pub static EVENT_ENDED: AtomicBool = AtomicBool::new(false);

pub fn active() -> bool {
    ACTIVE.load(Relaxed)
}
/// Id of the cutscene being started for the peer: its own state changes are neither sent back nor refused.
static APPLYING: AtomicU32 = AtomicU32::new(0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Phase,
    Camera,
}

impl Kind {
    pub(crate) fn of(act: &SceneAction) -> Option<Kind> {
        let g = game();
        if act.vtable == g.addr.abs(g.addr.phase_event_vtable) {
            Some(Kind::Phase)
        } else if act.vtable == g.addr.abs(g.addr.camera_action_vtable) {
            Some(Kind::Camera)
        } else {
            None
        }
    }

    /// A request that starts it (refused for one already played this session).
    fn starts(self, old: u32, new: u32) -> bool {
        match self {
            Kind::Phase => (new == phase::PLAY_REQ || new == phase::PLAYING) && !phase::STARTED.contains(&old),
            Kind::Camera => new == camera::PLAY && old != camera::PLAY,
        }
    }

    /// The change that tells the peer it started.
    fn announce(self, old: u32, new: u32) -> bool {
        match self {
            Kind::Phase => new == phase::PLAYING && old != phase::PLAYING,
            Kind::Camera => new == camera::PLAY && old != camera::PLAY,
        }
    }

    /// It already started or ran here.
    fn ran(self, state: u32) -> bool {
        match self {
            Kind::Phase => phase::STARTED.contains(&state) || state == phase::DONE,
            Kind::Camera => state != 0 && state != camera::WAIT,
        }
    }

    /// It is running now (the next one waits for it).
    fn running(self, state: u32) -> bool {
        match self {
            Kind::Phase => phase::STARTED.contains(&state),
            Kind::Camera => state == camera::PLAY,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Played {
    /// The action object it played on.
    act: usize,
    /// A cutscene reached DONE since: only then is a new start a second trigger. Before, it is the game's own
    /// next step (seen 2026-11-22: mid-play state b649891d -> PLAYING again on the client, wrongly refused).
    finished: bool,
}

#[derive(Default)]
struct Shared {
    /// Cutscenes already started this session, here or by the peer: id -> the action object it played on. A
    /// load (death, checkpoint) makes new action objects, whose triggers must work again (seen 2026-11-22: after
    /// a reload both games refused the camera event the area's script was waiting on).
    played: HashMap<u32, Played>,
    /// Ids whose refused re-trigger was logged.
    refused_logged: HashSet<u32>,
    /// Peer cutscenes waiting to start here: (id, received at ms).
    pending: Vec<(u32, f64)>,
    last_try_ms: f64,
    /// A cutscene we joined: (id, action, when to log its state); shows whether it kept playing.
    check: Option<(u32, usize, f64)>,
}

/// Never held across a call into the game: starting a cutscene runs `request_state`, which locks it again.
static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared::default()));

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn start() {
    *shared() = Shared::default();
    crate::sync::events::reset();
    crate::sync::quest::reset();
    LOGGED.store(0, Relaxed);
    ACTIVE.store(true, Relaxed);
}

pub fn end() {
    ACTIVE.store(false, Relaxed);
    *shared() = Shared::default();
    crate::sync::events::reset();
    crate::sync::quest::reset();
}

/// Our game reloaded (our character was recreated): every cutscene may play again.
pub fn forget_played() {
    let mut s = shared();
    if !s.played.is_empty() {
        tracing::info!("cutscene sync: our game reloaded; {} played cutscenes may trigger again", s.played.len());
    }
    s.played.clear();
    s.refused_logged.clear();
    drop(s);
    crate::sync::events::forget_contexts();
}

/// A cutscene or camera event (started by this module, not by forwarded signals).
pub(crate) fn is_cutscene(act: &SceneAction) -> bool {
    Kind::of(act).is_some()
}

/// `Action::request_state` detour (every scene action, game thread).
pub fn on_request(this: *mut SceneAction, req: *const StateReq) -> usize {
    // SAFETY: the engine passes a live action and request; both are null-checked.
    let (Some(act), Some(q)) = (unsafe { this.as_ref() }, unsafe { req.as_ref() }) else {
        return hooks::call_request_state(this, req);
    };
    let (id, old, hash) = (act.id, act.state, q.hash);
    let kind = Kind::of(act).filter(|_| ACTIVE.load(Relaxed) && APPLYING.load(Relaxed) != id);
    if let Some(k) = kind
        && k.starts(old, hash)
    {
        let mut s = shared();
        // A camera event only starts again after leaving PLAY (`starts` checks that); a cutscene once DONE.
        if s.played.get(&id).is_some_and(|p| p.act == this as usize && (k == Kind::Camera || p.finished)) {
            // Already seen (the other player triggered it, or we did) on this very action: don't play it again.
            if s.refused_logged.insert(id) {
                tracing::info!("cutscene sync: {k:?} {id:08x} already played this session; trigger ignored");
            }
            return 0;
        }
    }
    let r = hooks::call_request_state(this, req);
    if ACTIVE.load(Relaxed) && hash != old && LOGGED.load(Relaxed) < MAX_LOGGED {
        let g = game();
        let class = world::class_of_vtable(act.vtable).map_or("?".into(), |c| c.short_name());
        if !NOISY.iter().any(|n| class.starts_with(n)) {
            LOGGED.fetch_add(1, Relaxed);
            tracing::info!(
                "scene: action {id:08x} {class} (vt+{:#x}) {old:08x} -> {hash:08x} ok {}",
                act.vtable.wrapping_sub(g.addr.base),
                r & 0xFF
            );
        }
    }
    if hash == phase::DONE && (old == phase::PLAYING || old == phase::PLAY_END) && Kind::of(act) == Some(Kind::Phase) {
        // The cutscene suspended or re-AI'd the partner 2B (nier_coop `EVENT_ENDED`): `sync::player` sets her up again.
        EVENT_ENDED.store(true, Relaxed);
    }
    if ACTIVE.load(Relaxed) && hash != old {
        crate::sync::events::on_state_change(act.vtable, id, old, hash, q.flag);
    }
    if kind == Some(Kind::Phase)
        && hash == phase::DONE
        && let Some(p) = shared().played.get_mut(&id).filter(|p| p.act == this as usize)
    {
        p.finished = true;
    }
    if let Some(k) = kind
        && k.announce(old, hash)
        && shared().played.insert(id, Played { act: this as usize, finished: false }).is_none_or(|p| p.act != this as usize)
    {
        tracing::info!("cutscene sync: {k:?} {id:08x} started here; the other player joins it");
        state::send_event(Event::CutscenePlay { id });
    }
    r
}

/// The peer started cutscene `id`.
pub fn on_peer_play(id: u32, now: f64) {
    // Whether it already played here is decided in `tick`, against the action object loaded then.
    let mut s = shared();
    if !s.pending.iter().any(|&(p, _)| p == id) {
        tracing::info!("cutscene sync: other player started {id:08x}; joining");
        s.pending.push((id, now));
    }
}

/// Game thread, every frame: starts the peer's cutscenes, one at a time (starting one while another plays here
/// ran both and left the player stuck in event mode, nier_coop `play_pending`).
pub fn tick(now: f64) {
    let due = shared().check.filter(|c| now >= c.2);
    if let Some((id, act, _)) = due {
        shared().check = None;
        let here = find_by(|a| a.id == id).filter(|&p| p as usize == act);
        // SAFETY: still the same live action of the action scene.
        let st = here.map(|p| unsafe { (*p).state });
        tracing::info!("cutscene sync: {id:08x} one second after joining: state {:08x?}", st);
    }
    {
        let mut s = shared();
        if s.pending.is_empty() || now - s.last_try_ms < RETRY_EVERY_MS {
            return;
        }
        s.last_try_ms = now;
    }
    // SAFETY (closures below): `find_by` only hands out live actions of the action scene.
    if find_by(|a| Kind::of(a).is_some_and(|k| k.running(a.state))).is_some() {
        return; // the next one waits for this one to end
    }
    let pending = std::mem::take(&mut shared().pending);
    let mut keep = Vec::new();
    let mut started = false;
    for (id, since) in pending {
        if started {
            keep.push((id, since));
            continue;
        }
        let Some((act, kind)) = find_by(|a| a.id == id && Kind::of(a).is_some()).and_then(|p| {
            // SAFETY: `p` was just found in the live action scene.
            Kind::of(unsafe { &*p }).map(|k| (p, k))
        }) else {
            if now - since < PLAY_TIMEOUT_MS {
                keep.push((id, since)); // not loaded here yet
            } else {
                tracing::warn!("cutscene sync: {id:08x} never loaded here in {} s; skipped", PLAY_TIMEOUT_MS / 1000.0);
            }
            continue;
        };
        // SAFETY: as above.
        let st = unsafe { (*act).state };
        let prev = shared().played.insert(id, Played { act: act as usize, finished: false });
        if prev.is_some_and(|p| p.act == act as usize) {
            tracing::info!("cutscene sync: {kind:?} {id:08x} already played here");
            continue;
        }
        if kind.ran(st) {
            tracing::info!("cutscene sync: {kind:?} {id:08x} already started here (state {st:08x})");
            continue;
        }
        APPLYING.store(id, Relaxed);
        let ok = match kind {
            Kind::Phase => play_phase(act),
            Kind::Camera => request(act, camera::PLAY, 0),
        };
        APPLYING.store(0, Relaxed);
        tracing::info!("cutscene sync: playing {kind:?} {id:08x} (state {st:08x}): ok {ok}");
        shared().check = Some((id, act as usize, now + 1000.0));
        started = true;
    }
    let mut s = shared();
    keep.append(&mut s.pending); // events received meanwhile go after the older ones
    s.pending = keep;
}

/// `bool PhaseEventAction.play(this)` (script native).
fn play_phase(act: *mut SceneAction) -> u8 {
    let Some(play) = world::script_fn("PhaseEventAction.play") else {
        tracing::error!("cutscene sync: PhaseEventAction.play not found; cannot join cutscenes");
        return 0;
    };
    // SAFETY: the script native's signature, called on the game thread with a live action.
    unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(*mut SceneAction) -> u8>(play)(act) }
}

/// A state request for `hash` with its registered name, built as the engine builds it, passed to `f`; the name's
/// +1 reference from `build` is released afterwards (nier_coop `request`).
pub(crate) fn with_state_req<R>(hash: u32, flag: u32, f: impl FnOnce(&StateReq) -> R) -> R {
    let g = game();
    let mut q = StateReq::default();
    // SAFETY: resolved `StateReq* build(StateReq* out, u32 hash)`; `q` is a valid, large-enough out struct.
    unsafe {
        std::mem::transmute::<usize, unsafe extern "system" fn(*mut StateReq, u32) -> *mut StateReq>(g.addr.abs(g.addr.state_req_build))(
            &mut q, hash,
        )
    };
    q.flag = flag;
    let r = f(&q);
    let mut name = q.name as *mut u8;
    if !name.is_null() {
        // SAFETY: resolved `release(HashedStr**)`, releasing the reference `build` took.
        unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(*mut *mut u8)>(g.addr.abs(g.addr.str_release))(&mut name) };
    }
    r
}

/// `act.request_state(hash)`, bypassing our detour.
pub(crate) fn request(act: *mut SceneAction, hash: u32, flag: u32) -> u8 {
    with_state_req(hash, flag, |q| (hooks::call_request_state(act, q) & 0xFF) as u8)
}

/// First loaded scene action matching `f` (nier_coop `find_by`).
pub(crate) fn find_by(f: impl Fn(&SceneAction) -> bool) -> Option<*mut SceneAction> {
    let g = game();
    // SAFETY: the action scene is a resolved static in the module. Group and action pointers are null-checked
    // and the counts bounded before any element is read.
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
