//! Everything shared between the game thread, the net thread and the overlay. Nothing here holds a game
//! pointer: the net thread and the overlay never touch game memory (CLAUDE.md architecture rule 1).
//!
//! - [`Seqlock`]: single-writer "latest value" slot, lock-free for both sides (port of nier_coop `state.rs`,
//!   made generic).
//! - Bounded lock-free queues for discrete events in each direction.
//! - Small mutex-protected status structs for the overlay, which the game thread only `try_lock`s.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering, fence};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use bytemuck::Pod;
use crossbeam_queue::ArrayQueue;

use crate::net::protocol::{Datagram, EntityState, Event, PlayerSnapshot};

// -------------------------------------------------------------------------------------------------------
// Clock

static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Milliseconds since the mod loaded. Shared by every thread, so receive times and game time compare.
pub fn now_ms() -> f64 {
    START.elapsed().as_secs_f64() * 1000.0
}

// -------------------------------------------------------------------------------------------------------
// Seqlock

/// Single-writer seqlock holding the latest `T`. `T` is stored as `N` atomic u32 words, so a torn read is
/// detected and retried, never undefined behavior. `size_of::<T>()` must equal `4 * N` (checked when the
/// slot is constructed, at compile time for statics).
pub struct Seqlock<T, const N: usize> {
    seq: AtomicU32,
    words: [AtomicU32; N],
    _t: PhantomData<T>,
}

impl<T: Pod, const N: usize> Seqlock<T, N> {
    pub const fn new() -> Self {
        assert!(size_of::<T>() == 4 * N && align_of::<T>() <= 4);
        Self { seq: AtomicU32::new(0), words: [const { AtomicU32::new(0) }; N], _t: PhantomData }
    }

    /// Only one thread may write a given slot.
    pub fn store(&self, v: &T) {
        let n = self.seq.load(Ordering::Relaxed);
        self.seq.store(n.wrapping_add(1) | 1, Ordering::Relaxed);
        fence(Ordering::Release);
        let words: &[u32] = bytemuck::cast_slice(core::slice::from_ref(v));
        for (a, w) in self.words.iter().zip(words) {
            a.store(*w, Ordering::Relaxed);
        }
        // Even and never 0 again after the first store, so 0 keeps meaning "empty".
        self.seq.store((n | 1).wrapping_add(1).max(2), Ordering::Release);
    }

    /// Empties the slot (writer only).
    pub fn clear(&self) {
        self.seq.store(0, Ordering::Release);
    }

    /// `None` while empty.
    pub fn load(&self) -> Option<T> {
        loop {
            let a = self.seq.load(Ordering::Acquire);
            if a == 0 {
                return None;
            }
            if a % 2 == 1 {
                std::hint::spin_loop();
                continue;
            }
            let mut v = T::zeroed();
            let words: &mut [u32] = bytemuck::cast_slice_mut(core::slice::from_mut(&mut v));
            for (w, a) in words.iter_mut().zip(&self.words) {
                *w = a.load(Ordering::Relaxed);
            }
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == a {
                return Some(v);
            }
        }
    }
}

impl<T: Pod, const N: usize> Default for Seqlock<T, N> {
    fn default() -> Self {
        Self::new()
    }
}

// -------------------------------------------------------------------------------------------------------
// Session

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Role {
    None = 0,
    Host = 1,
    Client = 2,
}

/// Current session role, set by the net thread when a peer finishes the handshake.
static ROLE: AtomicU8 = AtomicU8::new(Role::None as u8);
/// Whether this session syncs enemies (the host decides).
pub static ENEMY_SYNC: AtomicBool = AtomicBool::new(false);

pub fn role() -> Role {
    match ROLE.load(Ordering::Acquire) {
        1 => Role::Host,
        2 => Role::Client,
        _ => Role::None,
    }
}

pub fn set_role(r: Role) {
    ROLE.store(r as u8, Ordering::Release);
}

// -------------------------------------------------------------------------------------------------------
// Game thread <-> net thread

/// Game -> net: the local player's latest state.
pub static LOCAL_PLAYER: Seqlock<PlayerSnapshot, { size_of::<PlayerSnapshot>() / 4 }> = Seqlock::new();

/// A snapshot with the local time it arrived.
#[derive(Clone, Copy, Debug)]
pub struct Received<T> {
    pub at_ms: f64,
    pub value: T,
}

/// Commands from the net thread to the game thread.
#[derive(Clone, Debug, PartialEq)]
pub enum GameCommand {
    /// A peer finished the handshake. The host then announces every live networked entity.
    SessionStarted { role: Role, enemy_sync: bool, peer_name: String },
    /// The peer left or timed out: restore single-player behavior.
    SessionEnded,
}

/// Commands from the overlay to the net thread.
#[derive(Clone, Debug, PartialEq)]
pub enum NetCommand {
    Host,
    Join(String),
    /// Mirror test: echo our own state back, shifted 2 m sideways.
    Loopback,
    Disconnect,
    Chat(String),
}

pub struct Queues {
    /// Game -> net: reliable events.
    pub out_events: ArrayQueue<Event>,
    /// Game -> net: datagrams other than the player snapshot (entity batches).
    pub out_datagrams: ArrayQueue<Datagram>,
    /// Net -> game.
    pub in_player: ArrayQueue<Received<PlayerSnapshot>>,
    pub in_entities: ArrayQueue<Received<Vec<EntityState>>>,
    pub in_events: ArrayQueue<Event>,
    pub game_cmds: ArrayQueue<GameCommand>,
    /// Overlay -> net.
    pub net_cmds: ArrayQueue<NetCommand>,
}

pub static Q: LazyLock<Queues> = LazyLock::new(|| Queues {
    out_events: ArrayQueue::new(4096),
    out_datagrams: ArrayQueue::new(256),
    in_player: ArrayQueue::new(256),
    in_entities: ArrayQueue::new(256),
    in_events: ArrayQueue::new(4096),
    game_cmds: ArrayQueue::new(64),
    net_cmds: ArrayQueue::new(64),
});

/// Queues an outgoing event; drops it with a warning if the net thread has fallen far behind.
pub fn send_event(e: Event) {
    if Q.out_events.push(e).is_err() {
        tracing::warn!("outgoing event queue full; event dropped");
    }
}

/// Empties every game <-> net queue (on session start and end), so nothing stale crosses sessions.
pub fn clear_session_queues() {
    while Q.out_events.pop().is_some() {}
    while Q.out_datagrams.pop().is_some() {}
    while Q.in_player.pop().is_some() {}
    while Q.in_entities.pop().is_some() {}
    while Q.in_events.pop().is_some() {}
}

// -------------------------------------------------------------------------------------------------------
// Overlay status

#[derive(Clone, Debug, Default)]
pub struct NetStatus {
    /// One-line state for the overlay ("Idle", "Hosting", "Connected", ...).
    pub state: String,
    pub join_code: Option<String>,
    pub peer_name: Option<String>,
    pub peer_version: Option<String>,
    pub rtt_ms: Option<f32>,
    /// "direct" or "relay".
    pub path: Option<&'static str>,
    pub last_error: Option<String>,
    pub busy: bool,
    pub connected: bool,
}

pub static NET_STATUS: LazyLock<Mutex<NetStatus>> =
    LazyLock::new(|| Mutex::new(NetStatus { state: "Idle".into(), ..Default::default() }));

pub fn net_status(f: impl FnOnce(&mut NetStatus)) {
    if let Ok(mut s) = NET_STATUS.lock() {
        f(&mut s);
    }
}

/// What the game thread publishes for drawing (name tag, debug panel).
#[derive(Clone, Debug, Default)]
pub struct OverlayInfo {
    /// Camera world transform rows (right, up, forward, origin) and vertical fov in radians.
    pub camera: Option<([[f32; 4]; 4], f32)>,
    /// World position above the remote player's head.
    pub remote_head: Option<[f32; 3]>,
    pub remote_hp: Option<u32>,
    pub entities: Vec<EntityInfo>,
    /// Field dump of the entity selected in the debug panel.
    pub inspect: Option<Inspect>,
    /// Enemy sync counters while a session syncs enemies.
    pub enemy: Option<EnemyStats>,
    pub game_ok: bool,
}

#[derive(Clone, Debug, Default)]
pub struct EntityInfo {
    pub handle: u32,
    pub name: String,
    pub class: String,
    pub pos: [f32; 3],
    pub hp: u32,
    /// Net id and how it is synced: "host" (ours, sent), "paired" (our own entity follows the host's),
    /// "copy" (spawned for the host's).
    pub net: Option<(u32, &'static str)>,
}

/// Enemy sync counters for the overlay.
#[derive(Clone, Debug, Default)]
pub struct EnemyStats {
    pub host: bool,
    /// Live networked entities (host: sent; client: host entities known).
    pub tracked: usize,
    /// Client: host entities backed by one of our own entities / by a copy we spawned / by nothing yet.
    pub paired: usize,
    pub copies: usize,
    pub waiting: usize,
    /// Client: our own entities removed because the host had no match.
    pub removed: u32,
    /// Client: host animations replayed / refused (not yet seen for that enemy class here).
    pub replayed: u32,
    pub refused: u32,
    /// Client: enemies killed because the host's died.
    pub kills: u32,
    /// Client: no host snapshot for a while (host loading?); our own AI runs our enemies meanwhile.
    pub host_stale: bool,
    /// Client: backed enemies the host's recent snapshots did not include (they run on our own AI).
    pub no_updates: usize,
    pub replay_unverified: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Inspect {
    pub handle: u32,
    pub class_chain: Vec<String>,
    /// (offset, u32, f32) rows of the first bytes of the behavior.
    pub rows: Vec<(usize, u32, f32)>,
}

/// Recent chat lines (sender: text), newest last. Written by the net thread, read by the overlay.
pub static CHAT: LazyLock<Mutex<std::collections::VecDeque<String>>> =
    LazyLock::new(|| Mutex::new(std::collections::VecDeque::new()));

pub fn push_chat(line: String) {
    if let Ok(mut c) = CHAT.lock() {
        c.push_back(line);
        while c.len() > 100 {
            c.pop_front();
        }
    }
}

pub static OVERLAY: LazyLock<Mutex<OverlayInfo>> = LazyLock::new(|| Mutex::new(OverlayInfo::default()));
/// The overlay is open: game input is blocked (see `proxy`).
pub static OVERLAY_VISIBLE: AtomicBool = AtomicBool::new(false);
/// Overlay -> game: the debug panel wants the entity list (0 = no) and which handle to inspect.
pub static DEBUG_LIST_WANTED: AtomicBool = AtomicBool::new(false);
pub static DEBUG_INSPECT: AtomicU32 = AtomicU32::new(0);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::cVec4;

    #[test]
    fn seqlock_concurrent() {
        static S: Seqlock<PlayerSnapshot, 20> = Seqlock::new();
        assert_eq!(S.load(), None);
        let w = std::thread::spawn(|| {
            for i in 1..=100_000u32 {
                let f = i as f32;
                S.store(&PlayerSnapshot { pos: cVec4::new(f, f, f, f), time_ms: i, hp: i, ..Default::default() });
            }
        });
        while !w.is_finished() {
            if let Some(s) = S.load() {
                // Every field written together is read together.
                assert_eq!(s.pos.x as u32, s.time_ms);
                assert_eq!(s.pos.w as u32, s.time_ms);
                assert_eq!(s.hp, s.time_ms);
            }
        }
        assert_eq!(S.load().unwrap().time_ms, 100_000);
        S.clear();
        assert_eq!(S.load(), None);
    }
}
