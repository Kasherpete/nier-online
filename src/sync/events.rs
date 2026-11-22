//! World event sync (the host's scripted world drives the client's): scene signals, `SendCommand` runs, quest
//! objectives and enemy-set spawns. Cutscene starts are `sync::scene`; story flags are `sync::quest`.
//!
//! Ported from nier_coop `src/hooks/events.rs` (GPL-3.0):
//! - **Signals** are how scripts trigger actions (an NPC talk that blows up debris, a door, a lift). The host's
//!   posts are hooked and sent; the client posts the same signal from its own action with that id. `Action.signal`
//!   cannot be used for that: outside the script engine it finds no sender context and sends from "nobody", which
//!   no listener reacts to. So each action's context is learned from the signals the client's own game posts.
//! - **SendCommand** (kind 1): when it reaches DONE on the host, the client runs its command list.
//! - **Objectives** (kind 2, `HapGoalLocationAction`): the host's state changes are mirrored (not ACTIVE: the
//!   objective's own update shows the HUD objective in WAIT, then requests ACTIVE itself). The client's player
//!   finishing one first finishes it on the host too.
//! - **Enemy sets** (kind 3, `EnemySetArea`): one spawns when the host's player enters its area; the client's
//!   never would, so the client starts its spawn once its own set has started (WAIT).
//!
//! Host events whose action is not loaded here yet are retried until it is (each game loads action groups by
//! itself; nier_coop saw the client load a boss-fight group 57 s after the host spawned its 26 enemy sets).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};

use crate::hooks::{self, game};
use crate::net::protocol::{Event, SCENE_ENEMY_SET, SCENE_GOAL, SCENE_MIRROR, SCENE_SEND_COMMAND};
use crate::sdk::{SceneAction, SignalSender, StateReq, action};
use crate::state::{self, Role};
use crate::sync::scene;

/// How long a host event waits for its action to load here.
const LOAD_TIMEOUT_MS: f64 = 180_000.0;
/// How often waiting events are retried (each try walks every loaded action).
const RETRY_EVERY_MS: f64 = 500.0;
/// How long a host enemy-set spawn waits for our set to start (WAIT).
const SPAWN_TIMEOUT_MS: f64 = 10_000.0;
const MAX_PENDING: usize = 512;
/// The same host signal is sent at most this often: actions post some signals every frame.
const SIGNAL_REPEAT_MS: f64 = 250.0;
/// Log lines for applied signals per session.
const MAX_SIGNAL_LOGS: u32 = 500;

/// Id of the action whose host event is being applied: its own state changes are not sent back.
static APPLYING: AtomicU32 = AtomicU32::new(0);
static SIGNAL_LOGS: AtomicU32 = AtomicU32::new(0);

#[derive(Default)]
struct Shared {
    /// Sender context last seen per action id, with the action object it came from. Areas stream in and out, so
    /// an id can be unloaded and loaded again with a new context: a context is only used while the action with
    /// that id is still the same object.
    contexts: HashMap<u32, (usize, usize, bool)>, // (context, action, it is a cutscene)
    /// Host: when each (sender, signal, argument) was last sent.
    sent_signals: HashMap<(u32, u32, u32), f64>,
    /// Client: host signals not yet posted here.
    signals: Vec<(u32, u32, u32)>,
    /// Host events not applied yet: ((id, kind, hash, flag), received at ms).
    pending: Vec<((u32, u8, u32, u32), f64)>,
    /// Client: enemy sets the host spawned, loaded here but not started yet: (id, received at ms).
    spawns: Vec<(u32, f64)>,
    last_retry_ms: f64,
}

/// Never held across a call into the game (posting and requesting states runs our own hooks again).
static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared::default()));

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

/// Session start or end.
pub fn reset() {
    *shared() = Shared::default();
    SIGNAL_LOGS.store(0, Relaxed);
}

/// Death or reload: drop the learned sender contexts, which an area unload frees (a stale one would be posted).
pub fn forget_contexts() {
    shared().contexts.clear();
}

fn host() -> bool {
    state::role() == Role::Host
}

/// Which synced kind an action is, by its class.
fn kind_of(vtable: usize) -> Option<u8> {
    let a = &game().addr;
    match vtable {
        v if v == a.abs(a.send_command_vtable) => Some(SCENE_SEND_COMMAND),
        v if v == a.abs(a.goal_vtable) => Some(SCENE_GOAL),
        v if v == a.abs(a.enemy_set_vtable) => Some(SCENE_ENEMY_SET),
        v if v == a.abs(a.enemy_set_action_vtable) || v == a.abs(a.combat_limit_vtable) || is_area(v) => Some(SCENE_MIRROR),
        _ => None,
    }
}

/// An area trigger: mirrored like a scene step, but not logged (they change as the host walks around).
fn is_area(vtable: usize) -> bool {
    let a = &game().addr;
    vtable == a.abs(a.area_action_vtable)
}

/// Our action with this layout id and kind.
fn find(id: u32, kind: u8) -> Option<*mut SceneAction> {
    scene::find_by(|a| a.id == id && kind_of(a.vtable) == Some(kind))
}

/// From `scene::on_request`, after a state change (session up). Sends what the peer must mirror.
pub fn on_state_change(vtable: usize, id: u32, old: u32, hash: u32, flag: u32) {
    let Some(kind) = kind_of(vtable) else { return };
    if APPLYING.load(Relaxed) == id {
        return;
    }
    let send = |what: &str| {
        if !is_area(vtable) {
            tracing::info!("world sync: {what} {id:08x} ({old:08x} -> {hash:08x})");
        }
        state::send_event(Event::SceneEvent { id, kind, hash, flag });
    };
    match kind {
        // A SendCommand reaches DONE right after running its commands (the explosion itself).
        SCENE_SEND_COMMAND if host() && hash == action::DONE => send("host ran SendCommand"),
        SCENE_GOAL if host() && hash != action::ACTIVE => send("host objective"),
        // The client's player reached an objective first: the host finishes it too (and sends what follows).
        SCENE_GOAL if !host() && hash == action::DONE => send("we finished objective"),
        SCENE_ENEMY_SET if host() && hash == action::SPAWN => send("host spawns enemy set"),
        // The host's script started (or moved on) a step of a scripted scene; the client's never would on its own.
        SCENE_MIRROR if host() => send("host scene step"),
        _ => {}
    }
}

/// Signal post detour (every scene signal, game thread).
pub fn on_post(queue: usize, from: *const SignalSender, sig: *const StateReq) -> usize {
    let r = hooks::call_signal_post(queue, from, sig);
    // SAFETY: the engine passes a valid sender and signal; both are null-checked.
    let (Some(f), Some(q)) = (unsafe { from.as_ref() }, unsafe { sig.as_ref() }) else { return r };
    if f.id == 0 || f.context == 0 || !scene::active() {
        return r;
    }
    // Learned only when new or changed: actions post signals every frame.
    let known = shared().contexts.get(&f.id).copied().filter(|&(c, _, _)| c == f.context);
    let from_cutscene = match known {
        Some((_, _, cutscene)) => cutscene,
        None => match scene::find_by(|a| a.id == f.id) {
            Some(act) => {
                // SAFETY: just found in the live action scene.
                let cutscene = scene::is_cutscene(unsafe { &*act });
                shared().contexts.insert(f.id, (f.context, act as usize, cutscene));
                cutscene
            }
            None => false,
        },
    };
    // A cutscene's or camera event's own signals are not sent: `sync::scene` starts those, and posting the host's
    // copies on the client's just-started camera event made it flicker and give the camera back (2026-11-22).
    if host() && !from_cutscene {
        let now = state::now_ms();
        let key = (f.id, q.hash, q.flag);
        let mut s = shared();
        if s.sent_signals.get(&key).is_none_or(|&t| now - t >= SIGNAL_REPEAT_MS) {
            s.sent_signals.insert(key, now);
            if s.sent_signals.len() > 4096 {
                s.sent_signals.retain(|_, t| now - *t < SIGNAL_REPEAT_MS);
            }
            drop(s);
            state::send_event(Event::SceneSignal { id: f.id, hash: q.hash, arg: q.flag });
        }
    }
    r
}

/// A host signal (client only).
pub fn on_peer_signal(id: u32, hash: u32, arg: u32) {
    if !host() {
        shared().signals.push((id, hash, arg));
    }
}

/// A peer scene event: everything from the host; from the client only a finished objective.
pub fn on_peer_event(id: u32, kind: u8, hash: u32, flag: u32, now: f64) {
    if host() && !(kind == SCENE_GOAL && hash == action::DONE) {
        return;
    }
    let mut s = shared();
    if s.pending.iter().any(|(e, _)| e.0 == id && e.1 == kind && e.2 == hash) {
        return;
    }
    if s.pending.len() >= MAX_PENDING {
        s.pending.remove(0);
    }
    s.pending.push(((id, kind, hash, flag), now));
    s.last_retry_ms = 0.0; // try at once
}

/// Game thread, every frame while a session runs.
pub fn tick(now: f64) {
    post_signals();
    let (retry, spawns) = {
        let mut s = shared();
        let retry = !s.pending.is_empty() && now - s.last_retry_ms >= RETRY_EVERY_MS;
        if retry {
            s.last_retry_ms = now;
        }
        (retry, !s.spawns.is_empty())
    };
    if spawns {
        spawn_pending(now);
    }
    if retry {
        apply_pending(now);
    }
}

/// Client: posts the host's signals from our own actions with the same ids.
fn post_signals() {
    let sigs = std::mem::take(&mut shared().signals);
    let g = game();
    for (id, hash, arg) in sigs {
        // A context is only trusted while the very action it was seen with is still loaded (an area unload frees
        // both; a reload of that id makes a new action with a new context).
        let entry = shared().contexts.get(&id).copied();
        let here = scene::find_by(|a| a.id == id).map(|p| p as usize);
        let Some((context, _, _)) = entry.filter(|&(_, act, _)| here == Some(act)) else {
            if entry.is_some() {
                shared().contexts.remove(&id); // reloaded or gone: its next own signal teaches the new context
            }
            continue;
        };
        scene::with_state_req(hash, arg, |q| hooks::call_signal_post(g.addr.abs(g.addr.signal_queue), &SignalSender::new(context, id), q));
        if SIGNAL_LOGS.fetch_add(1, Relaxed) < MAX_SIGNAL_LOGS {
            // SAFETY: `here` is the live action this context was learned from (checked just above).
            let class = here.and_then(|p| crate::sync::world::class_of_vtable(unsafe { (*(p as *const SceneAction)).vtable }));
            let class = class.map_or("?".into(), |c| c.short_name());
            tracing::info!("world sync: posted host signal {id:08x} ({class}) {hash:08x} arg {arg}");
        }
    }
}

fn apply_pending(now: f64) {
    let pending = std::mem::take(&mut shared().pending);
    let mut keep = Vec::new();
    for (ev, since) in pending {
        let (id, kind, ..) = ev;
        if apply(ev, now) {
            if now - since > RETRY_EVERY_MS * 2.0 {
                tracing::info!("world sync: {id:08x} (kind {kind}) loaded here {:.0} s after the host ran it; applied", (now - since) / 1000.0);
            }
        } else if now - since < LOAD_TIMEOUT_MS {
            keep.push((ev, since));
        } else {
            tracing::warn!("world sync: {id:08x} (kind {kind}) never loaded here in {} s; dropped", LOAD_TIMEOUT_MS / 1000.0);
        }
    }
    let mut s = shared();
    keep.append(&mut s.pending); // events received meanwhile go after the older ones
    s.pending = keep;
}

/// Calls vtable slot `slot` of `act` as `fn(this)`, if it points into the game module.
fn call_slot(act: *mut SceneAction, vt: usize, slot: usize) -> bool {
    let g = game();
    // SAFETY: `vt` is the resolved vtable of `act`'s class; the slot is read inside it and checked to be code in
    // the module before it is called with the live action.
    unsafe {
        let f = (vt as *const usize).add(slot).read();
        if !g.in_module(f) {
            return false;
        }
        std::mem::transmute::<usize, unsafe extern "system" fn(*mut SceneAction)>(f)(act);
    }
    true
}

/// Applies one host event; false when its action is not loaded here yet.
fn apply((id, kind, hash, flag): (u32, u8, u32, u32), now: f64) -> bool {
    let Some(act) = find(id, kind) else { return false };
    // SAFETY: `act` was just found in the live action scene.
    let (st, vt) = unsafe { ((*act).state, (*act).vtable) };
    APPLYING.store(id, Relaxed);
    match kind {
        SCENE_ENEMY_SET => {
            shared().spawns.push((id, now));
        }
        SCENE_GOAL if hash == action::DONE && st != action::DONE => {
            // Not a bare DONE request: finish also removes the map marker and HUD objective.
            call_slot(act, vt, action::GOAL_FINISH);
            tracing::info!("world sync: objective {id:08x} finished ({st:08x} -> {:08x})", unsafe { (*act).state });
        }
        SCENE_MIRROR if st != hash => {
            let ok = scene::request(act, hash, flag);
            if !is_area(vt) {
                tracing::info!("world sync: scene step {id:08x} {st:08x} -> {hash:08x}: ok {ok}");
            }
        }
        SCENE_GOAL if st != hash => {
            let ok = scene::request(act, hash, flag);
            tracing::info!("world sync: objective {id:08x} {st:08x} -> {hash:08x}: ok {ok}");
            // On the host the update runs right after WAIT; here the objective was never started, so nothing would
            // update it: run it once to show the objective (it moves itself to ACTIVE).
            if hash == action::WAIT {
                call_slot(act, vt, action::GOAL_UPDATE);
            }
        }
        SCENE_SEND_COMMAND if st != action::DONE => {
            let g = game();
            // SAFETY: slot `run(this, out)`, checked to be module code; it only writes a status word at +0x0C of
            // `out`, and 0x40 bytes is ample.
            unsafe {
                let f = (vt as *const usize).add(action::SEND_COMMAND_RUN).read();
                if g.in_module(f) {
                    let mut out = [0u8; 0x40];
                    std::mem::transmute::<usize, unsafe extern "system" fn(*mut SceneAction, *mut u8)>(f)(act, out.as_mut_ptr());
                }
            }
            tracing::info!("world sync: ran SendCommand {id:08x} (state {st:08x} -> {:08x})", unsafe { (*act).state });
        }
        _ => {}
    }
    APPLYING.store(0, Relaxed);
    if kind == SCENE_ENEMY_SET {
        spawn_pending(now);
    }
    true
}

/// Client: starts the spawn of the enemy sets the host spawned, once each has started here (WAIT); drops each
/// after `SPAWN_TIMEOUT_MS`, or when it spawned by itself.
fn spawn_pending(now: f64) {
    let spawns = std::mem::take(&mut shared().spawns);
    let mut keep = Vec::new();
    for (id, since) in spawns {
        let Some(act) = find(id, SCENE_ENEMY_SET) else {
            if now - since < SPAWN_TIMEOUT_MS {
                keep.push((id, since));
            }
            continue;
        };
        // SAFETY: as in `apply`.
        let st = unsafe { (*act).state };
        if st == action::WAIT || st == action::SPAWN_DELAY {
            APPLYING.store(id, Relaxed);
            let ok = scene::request(act, action::SPAWN, 0);
            APPLYING.store(0, Relaxed);
            tracing::info!("world sync: enemy set {id:08x} {st:08x} -> spawn: ok {ok}");
        } else if st == 0 && now - since < SPAWN_TIMEOUT_MS {
            keep.push((id, since)); // not started here yet: its start would request WAIT over our spawn
        } else if st == 0 {
            tracing::warn!("world sync: enemy set {id:08x} never started here");
        }
    }
    shared().spawns.append(&mut keep);
}
