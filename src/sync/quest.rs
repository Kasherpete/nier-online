//! Quest sync: the host's scene states ("q120/DONE", "q410/REQ", "Route/A", ...: the story and quest progress the
//! save keeps under SceneState) are copied to the client through the game's own `SceneStateSystem.set` / `.reset`,
//! the functions its scripts call. Game thread only. Ported from nier_coop `src/hooks/quest.rs` (GPL-3.0).

use std::collections::{HashMap, HashSet};
use std::sync::{LazyLock, Mutex};

use crate::hooks::game;
use crate::net::protocol::{Event, MAX_SCENE_STATE, MAX_SCENE_STATES};
use crate::sdk::{HashedStr, SceneStateSystem};
use crate::state::{self, Role};

/// States that describe this machine or this player's character, not the shared story.
const LOCAL_ONLY: [&str; 5] = ["Buddy/", "PL/", "DLC_", "playgo", "auto/"]; // auto/blocked: autosave lock
/// Host: list check interval, and the longest gap between two sends of an unchanged list.
const CHECK_EVERY_MS: f64 = 1000.0;
const RESEND_EVERY_MS: f64 = 3000.0;
/// Lists in a row a state must be missing on the host before it is reset here.
const RESET_AFTER: u32 = 3;

#[derive(Default)]
struct Shared {
    last_check_ms: f64,
    last_sent_ms: f64,
    last_sent: Vec<String>,
    /// Client: the newest host list, applied by `tick`.
    incoming: Option<Vec<String>>,
    /// Client: per state we have and the host lacks, host lists in a row without it.
    missing: HashMap<String, u32>,
}

static SHARED: LazyLock<Mutex<Shared>> = LazyLock::new(|| Mutex::new(Shared::default()));

fn shared() -> std::sync::MutexGuard<'static, Shared> {
    SHARED.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn reset() {
    *shared() = Shared::default();
}

/// The host's list arrived (validated by `decode_event`).
pub fn on_states(names: Vec<String>) {
    if state::role() != Role::Host {
        shared().incoming = Some(names);
    }
}

fn valid(n: &str) -> bool {
    !n.is_empty() && n.len() <= MAX_SCENE_STATE && n.bytes().all(|c| c.is_ascii_graphic())
}

fn text(p: *const u8) -> Option<String> {
    if p.is_null() {
        return None;
    }
    let mut v = Vec::new();
    for i in 0..=MAX_SCENE_STATE {
        // SAFETY: a NUL-terminated engine string; at most MAX_SCENE_STATE + 1 bytes are read.
        match unsafe { p.add(i).read() } {
            0 => break,
            c => v.push(c),
        }
    }
    String::from_utf8(v).ok().filter(|n| valid(n))
}

/// Our current scene states.
fn states() -> Vec<String> {
    let g = game();
    // SAFETY: the resolved static SceneStateSystem; its array pointer is null-checked and the count bounded.
    unsafe {
        let Some(s) = (g.addr.abs(g.addr.scene_states) as *const SceneStateSystem).as_ref() else { return Vec::new() };
        if s.states.is_null() || s.count > MAX_SCENE_STATES {
            return Vec::new();
        }
        (0..s.count).filter_map(|i| s.states.add(i).read().name.as_ref().and_then(|h: &HashedStr| text(h.text))).collect()
    }
}

/// Shared states the host has and we lack (to set), and that we have and the host does not (to reset).
fn diff<'a>(ours: &[&'a str], theirs: &[&'a str]) -> (Vec<&'a str>, Vec<&'a str>) {
    let shared = |n: &&str| !LOCAL_ONLY.iter().any(|p| n.starts_with(p));
    let o: HashSet<&str> = ours.iter().copied().filter(shared).collect();
    let t: HashSet<&str> = theirs.iter().copied().filter(shared).collect();
    let mut add: Vec<_> = t.difference(&o).copied().collect();
    let mut del: Vec<_> = o.difference(&t).copied().collect();
    add.sort_unstable();
    del.sort_unstable();
    (add, del)
}

/// States to reset now: missing from the host's list `RESET_AFTER` times in a row (a state set here a moment
/// before the host's next list shows it, or a host list taken mid-load, made states flap). A `partial` list counts
/// toward nothing.
fn debounce<'a>(missing: &mut HashMap<String, u32>, del: &[&'a str], partial: bool) -> Vec<&'a str> {
    missing.retain(|n, _| del.contains(&n.as_str()));
    if partial {
        return Vec::new();
    }
    let mut out = Vec::new();
    for &n in del {
        let c = missing.entry(n.to_string()).or_insert(0);
        *c += 1;
        if *c >= RESET_AFTER {
            missing.remove(n);
            out.push(n);
        }
    }
    out
}

/// `f(&sys, &name)` with `name` interned the way the game's own call sites do it.
fn call(f: usize, name: &str) -> bool {
    let a = &game().addr;
    let z: Vec<u8> = name.bytes().chain([0]).collect(); // intern copies len + 1 bytes
    // SAFETY: resolved string-pool functions and a SceneStateSystem native, with the signatures nier_coop verified.
    unsafe {
        let hash = std::mem::transmute::<usize, unsafe extern "system" fn(*const u8, usize) -> u32>(a.abs(a.str_hash));
        let intern = std::mem::transmute::<usize, unsafe extern "system" fn(usize, u32, *const u8, usize) -> *mut HashedStr>(a.abs(a.str_intern));
        let release = std::mem::transmute::<usize, unsafe extern "system" fn(*mut *mut HashedStr)>(a.abs(a.str_release));
        let set = std::mem::transmute::<usize, unsafe extern "system" fn(usize, *const *mut HashedStr) -> u8>(a.abs(f));
        let mut s = intern(a.abs(a.str_pool), hash(z.as_ptr(), name.len()), z.as_ptr(), name.len());
        if s.is_null() {
            return false;
        }
        let ok = set(a.abs(a.scene_states), &s) != 0;
        release(&mut s);
        ok
    }
}

/// Game thread, every frame while a session runs.
pub fn tick(now: f64) {
    if state::role() == Role::Host {
        let mut s = shared();
        if now - s.last_check_ms < CHECK_EVERY_MS {
            return;
        }
        s.last_check_ms = now;
        drop(s);
        let ours = states();
        let mut s = shared();
        if !ours.is_empty() && (ours != s.last_sent || now - s.last_sent_ms >= RESEND_EVERY_MS) {
            s.last_sent_ms = now;
            s.last_sent = ours.clone();
            drop(s);
            state::send_event(Event::SceneStates { names: ours });
        }
        return;
    }
    let Some(theirs) = shared().incoming.take() else { return };
    let ours = states();
    // An empty list is a host between saves (title screen, loading), never "erase all progress".
    if theirs.is_empty() || ours.is_empty() {
        return;
    }
    let (o, t): (Vec<&str>, Vec<&str>) = (ours.iter().map(String::as_str).collect(), theirs.iter().map(String::as_str).collect());
    let (add, del) = diff(&o, &t);
    let a = &game().addr;
    for n in add {
        if call(a.scene_set, n) {
            tracing::info!("quest sync: + {n}");
        }
    }
    let partial = t.len() * 2 < o.len();
    let resets = debounce(&mut shared().missing, &del, partial);
    for n in resets {
        if call(a.scene_reset, n) {
            tracing::info!("quest sync: - {n}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_skips_local_states() {
        let ours = ["q120/RUN_1", "Buddy/2B", "PL/2B", "Route/A", "auto/blocked"];
        let theirs = ["q120/DONE", "Buddy/Auto", "Route/A", "DLC_installed"];
        assert_eq!(diff(&ours, &theirs), (vec!["q120/DONE"], vec!["q120/RUN_1"]));
    }

    #[test]
    fn resets_only_after_repeated_absence() {
        let mut m = HashMap::new();
        assert!(debounce(&mut m, &["q1", "q2"], false).is_empty());
        assert!(debounce(&mut m, &["q1"], false).is_empty());
        assert!(debounce(&mut m, &["q1", "q2"], true).is_empty());
        assert_eq!(debounce(&mut m, &["q1", "q2"], false), vec!["q1"]);
    }
}
