//! Host: enemies treat the nearer of the two players as "the player".
//!
//! Enemy AI (and boss AI) asks the engine for its target through three small getters that read the
//! possessed handle: `get_player`, `get_player_entity` and `get_player_pos` (build 7788: 0x7c4f50, 0x7c5020,
//! 0x7c5050; see `notes/2026-11-22-enemy-targeting-reversing.md`). So an enemy only ever noticed the host's
//! character: with the host away, it stayed passive even with the client's puppet standing next to it,
//! until the puppet hit it. The client mirrors what the host's enemy does, so it stayed passive there too.
//!
//! Every enemy class's update slots are wrapped (`hooks::wrap_enemy_updates`). While one enemy updates, it
//! gets a target choice here, and the getters return the puppet instead of the host's character when the
//! puppet is the nearer one. Outside enemy updates, and on the client, the getters behave as before.
//! The choice is per thread (the engine may update entities on several threads) and keeps a margin, so an
//! enemy between the two players does not flip targets every frame.

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::ptr::addr_of;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};
use std::sync::{LazyLock, Mutex};

use crate::hooks;
use crate::sdk::{BehaviorAppBase, cVec4};
use crate::state::{self, Role};
use crate::sync::player;

/// The other player must be this much nearer (m) before an enemy switches to them.
const SWITCH_MARGIN: f32 = 3.0;
/// Size cap of the per-enemy choice map (entries of removed enemies are not tracked).
const MAX_CHOICES: usize = 4096;

/// Setting `enemy_target_client`.
static ENABLED: AtomicBool = AtomicBool::new(true);
/// Enemies switched to the puppet this session, for one log line each time the count grows past a power of 2.
static SWITCHES: AtomicU32 = AtomicU32::new(0);

thread_local! {
    /// Handle of the puppet while the enemy updating on this thread targets it; 0 otherwise.
    static TARGET: Cell<u32> = const { Cell::new(0) };
}

/// Per enemy (behavior address): does it target the puppet.
static CHOICE: LazyLock<Mutex<HashMap<usize, bool>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Relaxed);
}

/// Session start / end: forget old choices.
pub fn reset() {
    if let Ok(mut m) = CHOICE.lock() {
        m.clear();
    }
    SWITCHES.store(0, Relaxed);
}

/// Host: whether the enemy behind `b` currently targets the client (for the overlay).
pub fn targets_client(b: usize) -> bool {
    CHOICE.try_lock().is_ok_and(|m| m.get(&b).copied().unwrap_or(false))
}

/// Pure choice with hysteresis: `was` = the enemy targeted the puppet last time.
fn prefer_puppet(was: bool, to_host: f32, to_puppet: f32) -> bool {
    if was { to_puppet <= to_host + SWITCH_MARGIN } else { to_puppet + SWITCH_MARGIN < to_host }
}

/// The puppet's handle if the enemy `em` should target it this update, else 0.
fn choose(em: *const BehaviorAppBase) -> u32 {
    if !ENABLED.load(Relaxed) || state::role() != Role::Host || em.is_null() {
        return 0;
    }
    let (Some(local), Some((remote, _, handle))) = (player::live_local(), player::live_remote()) else { return 0 };
    // SAFETY: `em` is the object whose EmBase vtable called the update (a BehaviorAppBase prefix); the two
    // players were just checked to be live.
    let (at, host, puppet) = unsafe {
        (addr_of!((*em).position).read(), addr_of!((*local).base.position).read(), addr_of!((*remote).base.position).read())
    };
    let (dh, dp) = (at.dist(host), at.dist(puppet));
    if !(dh.is_finite() && dp.is_finite()) {
        return 0;
    }
    // Never wait on the lock inside the game's update; without it, decide without hysteresis.
    let now = match CHOICE.try_lock() {
        Ok(mut m) => {
            let was = m.get(&(em as usize)).copied().unwrap_or(false);
            let now = prefer_puppet(was, dh, dp);
            if m.len() >= MAX_CHOICES {
                m.clear();
            }
            m.insert(em as usize, now);
            if now && !was {
                let n = SWITCHES.fetch_add(1, Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("enemy targeting: {n} enemies switched to the other player so far (nearer to them)");
                }
            }
            now
        }
        Err(_) => prefer_puppet(false, dh, dp),
    };
    if now { handle } else { 0 }
}

/// Runs one enemy update (`f`) with that enemy's target choice in effect for the getters.
pub fn during_update<R>(this: *mut c_void, f: impl FnOnce() -> R) -> R {
    let t = choose(this.cast());
    let prev = TARGET.try_with(|c| c.replace(t)).unwrap_or(0);
    let r = f();
    let _ = TARGET.try_with(|c| c.set(prev));
    r
}

/// The puppet, if the enemy updating on this thread targets it and it is still alive.
fn puppet_target() -> Option<(*mut crate::sdk::Pl0000, *mut crate::sdk::Entity)> {
    let h = TARGET.try_with(Cell::get).unwrap_or(0);
    if h == 0 {
        return None;
    }
    let (b, e, live) = player::live_remote()?;
    (live == h).then_some((b, e))
}

pub fn get_player() -> *mut c_void {
    match puppet_target() {
        Some((b, _)) => b.cast(),
        None => hooks::call_get_player(),
    }
}

pub fn get_player_entity() -> *mut c_void {
    match puppet_target() {
        Some((_, e)) => e.cast(),
        None => hooks::call_get_player_entity(),
    }
}

pub fn get_player_pos(out: *mut cVec4) -> *mut cVec4 {
    match (puppet_target(), out.is_null()) {
        (Some((b, _)), false) => {
            // SAFETY: `out` is the caller's vector (non-null); `b` is the live remote Pl0000.
            unsafe { out.write_unaligned(addr_of!((*b).base.position).read()) };
            out
        }
        _ => hooks::call_get_player_pos(out),
    }
}

#[cfg(test)]
mod tests {
    use super::prefer_puppet;

    #[test]
    fn switches_only_with_a_margin() {
        assert!(!prefer_puppet(false, 10.0, 8.0)); // nearer, but not by enough
        assert!(prefer_puppet(false, 10.0, 6.0));
        assert!(prefer_puppet(true, 10.0, 12.0)); // stays on the puppet while the host is not clearly nearer
        assert!(!prefer_puppet(true, 10.0, 14.0));
        assert!(prefer_puppet(false, 200.0, 2.0)); // host far away (the reported case)
    }
}
