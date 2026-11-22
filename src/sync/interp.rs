//! Playback of a remote stream of timestamped snapshots and events, a fixed delay behind real time.
//!
//! nier_coop applied the newest packet directly (moving halfway there each frame) and carried only the
//! *last* animation and button press per packet, so bursts of inputs were lost and movement rubber-banded
//! with network jitter. Here every snapshot and event carries the sender's clock, and the receiver plays
//! them all back on one shared timeline `DELAY_MS` behind the sender: positions are interpolated between
//! the two snapshots around the playback time, and events fire, in order, when the playback time passes
//! them. Inputs and positions therefore stay consistent with each other.
//!
//! Pure logic (no game memory), so it is unit-tested here.

use std::collections::VecDeque;
use std::f32::consts::{PI, TAU};

use crate::net::protocol::PlayerSnapshot;
use crate::sdk::cVec4;

/// How far behind the sender the playback runs. Covers jitter up to about this much.
pub const DELAY_MS: f64 = 100.0;
/// Without snapshots for this long, the peer counts as gone.
pub const STALE_MS: f64 = 2000.0;
const MAX_SNAPSHOTS: usize = 64;
const MAX_EVENTS: usize = 1024;

/// Estimates `local_clock - remote_clock` from arrival times. The minimum over arrivals is the estimate
/// with the least network delay; it drifts slowly upward so a one-off fast packet or clock drift does not
/// pin it forever.
#[derive(Debug, Default, Clone, Copy)]
struct ClockOffset(Option<f64>);

impl ClockOffset {
    fn observe(&mut self, local_ms: f64, remote_ms: f64) {
        let o = local_ms - remote_ms;
        self.0 = Some(match self.0 {
            Some(cur) if o >= cur => cur + (o - cur) * 0.01,
            _ => o,
        });
    }
}

#[derive(Debug)]
pub struct Timeline<E> {
    snaps: VecDeque<PlayerSnapshot>,
    events: VecDeque<(u32, E)>,
    offset: ClockOffset,
    last_rx: Option<f64>,
}

/// The remote player's state at the playback time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    /// The latest snapshot at or before the playback time: its discrete fields (inputs, hp) apply as-is.
    pub base: PlayerSnapshot,
    /// Interpolated between `base` and the next snapshot.
    pub pos: cVec4,
    pub rot: cVec4,
}

fn lerp_angle(a: f32, b: f32, t: f32) -> f32 {
    let mut d = (b - a) % TAU;
    if d > PI {
        d -= TAU;
    } else if d < -PI {
        d += TAU;
    }
    a + d * t
}

impl<E> Default for Timeline<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> Timeline<E> {
    pub fn new() -> Self {
        Self { snaps: VecDeque::new(), events: VecDeque::new(), offset: ClockOffset(None), last_rx: None }
    }

    pub fn push_snapshot(&mut self, local_ms: f64, s: PlayerSnapshot) {
        self.offset.observe(local_ms, s.time_ms as f64);
        self.last_rx = Some(local_ms);
        // Keep sorted by sender time; drop duplicates (unreliable datagrams can repeat or reorder).
        let i = self.snaps.partition_point(|x| x.time_ms < s.time_ms);
        if self.snaps.get(i).is_some_and(|x| x.time_ms == s.time_ms) {
            return;
        }
        self.snaps.insert(i, s);
        while self.snaps.len() > MAX_SNAPSHOTS {
            self.snaps.pop_front();
        }
    }

    /// Queues an event stamped with the sender's clock. Events arrive in order on the reliable stream.
    pub fn push_event(&mut self, time_ms: u32, e: E) {
        self.events.push_back((time_ms, e));
        while self.events.len() > MAX_EVENTS {
            self.events.pop_front();
        }
    }

    /// Playback time on the sender's clock, once the clock offset is known.
    pub fn playback_ms(&self, local_ms: f64) -> Option<f64> {
        self.offset.0.map(|o| local_ms - o - DELAY_MS)
    }

    pub fn is_stale(&self, local_ms: f64) -> bool {
        self.last_rx.is_none_or(|t| local_ms - t > STALE_MS)
    }

    /// Events whose time has come, oldest first.
    pub fn due_events(&mut self, local_ms: f64) -> Vec<E> {
        let Some(t) = self.playback_ms(local_ms) else { return Vec::new() };
        let mut out = Vec::new();
        while self.events.front().is_some_and(|(at, _)| (*at as f64) <= t) {
            out.push(self.events.pop_front().unwrap().1);
        }
        out
    }

    /// State at the playback time. Before the first snapshot it is the oldest one; past the newest it holds
    /// the newest (no extrapolation: guessing ahead looks worse than a brief pause).
    pub fn sample(&mut self, local_ms: f64) -> Option<Sample> {
        let t = self.playback_ms(local_ms)?;
        // Drop snapshots that both precede the playback time, keeping the one right before it.
        while self.snaps.len() >= 2 && (self.snaps[1].time_ms as f64) <= t {
            self.snaps.pop_front();
        }
        let a = *self.snaps.front()?;
        let Some(b) = self.snaps.get(1).copied().filter(|_| (a.time_ms as f64) <= t) else {
            return Some(Sample { base: a, pos: a.pos, rot: a.rot });
        };
        let span = (b.time_ms - a.time_ms) as f64;
        let k = (((t - a.time_ms as f64) / span) as f32).clamp(0.0, 1.0);
        let mut rot = a.rot.lerp(b.rot, k);
        rot.y = lerp_angle(a.rot.y, b.rot.y, k);
        Some(Sample { base: a, pos: a.pos.lerp(b.pos, k), rot })
    }

    pub fn clear(&mut self) {
        *self = Self::new();
    }
}

/// Moves `cur` toward `target`: snap if the error is over `snap_m` metres, otherwise close the gap
/// exponentially (about 12 per second), so small corrections never look like teleports.
pub fn correct(cur: cVec4, target: cVec4, dt_s: f32, snap_m: f32) -> cVec4 {
    if !cur.is_sane() || cur.dist(target) > snap_m {
        return target;
    }
    let k = 1.0 - (-12.0 * dt_s.clamp(0.0, 0.25)).exp();
    let mut p = cur.lerp(target, k);
    p.w = target.w;
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(t: u32, x: f32) -> PlayerSnapshot {
        PlayerSnapshot { time_ms: t, pos: cVec4::new(x, 0.0, 0.0, 1.0), ..Default::default() }
    }

    #[test]
    fn interpolates_at_a_delay() {
        let mut tl = Timeline::<()>::new();
        // Sender clock starts at 1000; it arrives 50 ms later on our clock (offset 50 -> -950).
        tl.push_snapshot(1050.0, snap(1000, 0.0));
        tl.push_snapshot(1150.0, snap(1100, 10.0));
        // Local 1200 -> sender time 1200 - 50 - 100 = 1050: halfway between.
        let s = tl.sample(1200.0).unwrap();
        assert!((s.pos.x - 5.0).abs() < 1e-4, "{s:?}");
        assert_eq!(s.base.time_ms, 1000);
        // Past the newest: hold it.
        assert_eq!(tl.sample(5000.0).unwrap().pos.x, 10.0);
    }

    #[test]
    fn ignores_duplicates_and_reorders() {
        let mut tl = Timeline::<()>::new();
        tl.push_snapshot(0.0, snap(100, 1.0));
        tl.push_snapshot(0.0, snap(300, 3.0));
        tl.push_snapshot(0.0, snap(200, 2.0)); // late
        tl.push_snapshot(0.0, snap(200, 9.0)); // duplicate
        let xs: Vec<_> = tl.snaps.iter().map(|s| s.pos.x).collect();
        assert_eq!(xs, vec![1.0, 2.0, 3.0]);
    }

    #[test]
    fn events_fire_in_order_when_due() {
        let mut tl = Timeline::<u32>::new();
        tl.push_event(1000, 1);
        assert!(tl.due_events(0.0).is_empty()); // offset unknown yet: keep waiting
        tl.push_snapshot(1000.0, snap(1000, 0.0)); // offset 0
        tl.push_event(1050, 2);
        tl.push_event(1200, 3);
        assert_eq!(tl.due_events(1100.0), vec![1]); // playback 1000
        assert_eq!(tl.due_events(1160.0), vec![2]); // playback 1060
        assert_eq!(tl.due_events(1400.0), vec![3]);
    }

    #[test]
    fn offset_takes_fastest_arrival() {
        let mut c = ClockOffset::default();
        c.observe(150.0, 100.0); // 50
        c.observe(220.0, 200.0); // 20: faster, take it
        c.observe(400.0, 300.0); // 100: slower, drift up only a little
        assert!((c.0.unwrap() - 20.8).abs() < 1e-6);
    }

    #[test]
    fn yaw_wraps() {
        assert!((lerp_angle(3.0, -3.0, 0.5).abs() - PI).abs() < 0.01);
        assert!((lerp_angle(0.0, 1.0, 0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn correction_blends_or_snaps() {
        let a = cVec4::new(0.0, 0.0, 0.0, 1.0);
        let near = cVec4::new(1.0, 0.0, 0.0, 1.0);
        let p = correct(a, near, 1.0 / 60.0, 3.0);
        assert!(p.x > 0.0 && p.x < 1.0);
        let far = cVec4::new(10.0, 0.0, 0.0, 1.0);
        assert_eq!(correct(a, far, 1.0 / 60.0, 3.0), far);
        assert_eq!(correct(cVec4::new(f32::NAN, 0.0, 0.0, 0.0), near, 0.016, 3.0), near);
    }

    #[test]
    fn stale_after_silence() {
        let mut tl = Timeline::<()>::new();
        assert!(tl.is_stale(0.0));
        tl.push_snapshot(100.0, snap(0, 0.0));
        assert!(!tl.is_stale(2000.0));
        assert!(tl.is_stale(2101.0));
    }
}
