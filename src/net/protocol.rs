//! Wire protocol. Two message types share one QUIC connection:
//! - [`Datagram`]: unreliable, latest-wins snapshots (players, enemies), each with a sequence number.
//! - [`Event`]: reliable and ordered, on one bidirectional stream, framed as `u32 LE length + postcard`.
//!
//! Everything received is untrusted (CLAUDE.md rule 4): [`decode_datagram`] and [`decode_event`] enforce
//! exact lengths, reject non-finite or absurd numbers and over-long strings or lists, and never produce
//! anything that could become a pointer.

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

use crate::sdk::{self, cVec4};

/// Bumped on any change to the message shapes below. Both sides must match exactly.
pub const PROTOCOL_VERSION: u16 = 2;
/// QUIC application protocol id. Kept at /1 across protocol versions, so a peer on another version still
/// connects and gets the clear refusal from the `Hello` check instead of a bare connection error.
pub const ALPN: &[u8] = b"nier-online/1";
/// Largest datagram we send or accept. QUIC datagrams must fit one packet (~1200 bytes minimum MTU).
pub const MAX_DATAGRAM: usize = 1100;
/// Largest event frame we send or accept.
pub const MAX_FRAME: usize = 64 * 1024;
/// Entity states per [`Datagram::Entities`]: 24 * ~40 bytes stays under [`MAX_DATAGRAM`].
pub const ENTITIES_PER_DATAGRAM: usize = 24;

pub const MAX_NAME: usize = 32;
pub const MAX_CHAT: usize = 256;
/// Entity names live in a 0x20-byte field with a NUL terminator.
pub const MAX_ENTITY_NAME: usize = 31;
pub const MAX_HP: u32 = 10_000_000;
/// Animation ids above this are not real animations; replaying garbage ids can crash the engine.
pub const MAX_ANIM: u32 = 4096;

/// One player's state, sampled once per game frame by its owner.
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable, Serialize, Deserialize)]
pub struct PlayerSnapshot {
    pub time_ms: u32,      // 0x00 sender's session clock
    pub flags: u32,        // 0x04 FLAG_*
    pub pos: cVec4,        // 0x08
    pub rot: cVec4,        // 0x18 Model rotation; .y is yaw
    pub facing2: f32,      // 0x28 controller stick direction
    pub speed: f32,        // 0x2C controller stick magnitude
    pub held: u32,         // 0x30 controller held_flags
    pub hp: u32,           // 0x34
    pub aim: cVec4,        // 0x38 pod aim point
    pub pod_cooldown: f32, // 0x48 active pod's remaining program cooldown
    pub model: u32,        // 0x4C EModel of the character (2B / 9S / A2)
}
const _: () = assert!(size_of::<PlayerSnapshot>() == 0x50);

impl PlayerSnapshot {
    /// The sender controls a `Pl0000` this frame (false in flight units, hacking, menus).
    pub const FLAG_PL0000: u32 = 1 << 0;
    /// Pod light on.
    pub const FLAG_FLASHLIGHT: u32 = 1 << 1;
}

/// A host-owned entity's state.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntityState {
    pub net_id: u32,
    pub pos: cVec4,
    pub rot: cVec4,
    pub hp: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Datagram {
    Player { seq: u32, snap: PlayerSnapshot },
    Entities { seq: u32, time_ms: u32, items: Vec<EntityState> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u16,
    pub mod_version: String,
    /// PE timestamp of the game exe.
    pub game_build: u32,
    pub name: String,
    /// Host: whether this session syncs enemies. Client: ignored.
    pub enemy_sync: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SpawnEntity {
    pub net_id: u32,
    pub name: String,
    pub model: u32,
    pub pos: cVec4,
    pub rot: cVec4,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// First message each way. The host answers a client's `Hello` with its own or with `Refuse`.
    Hello(Hello),
    Refuse { reason: String },
    /// The sender's character started an animation (`Behavior::start_animation` arguments).
    Anim { time_ms: u32, args: [u32; 4] },
    /// The sender's controller had button presses this frame.
    Buttons { time_ms: u32, buttons: [u32; 8] },
    /// Host -> client: spawn a host-owned entity.
    SpawnEntity(SpawnEntity),
    /// Host -> client: a host-owned entity started an animation (`EmBase::start_animation` arguments).
    EntityAnim { net_id: u32, args: [u32; 4] },
    /// Host -> client: a host-owned entity died (its die state started, or its HP reached 0).
    EntityDied { net_id: u32 },
    /// Host -> client: a host-owned entity was removed from the world.
    DestroyEntity { net_id: u32 },
    Chat { text: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    TooLong(usize),
    Malformed,
    TrailingBytes,
    Invalid(&'static str),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooLong(n) => write!(f, "message too long ({n} bytes)"),
            Self::Malformed => f.write_str("malformed message"),
            Self::TrailingBytes => f.write_str("trailing bytes after message"),
            Self::Invalid(why) => write!(f, "invalid message: {why}"),
        }
    }
}

fn decode<'a, T: Deserialize<'a>>(b: &'a [u8], max: usize) -> Result<T, DecodeError> {
    if b.len() > max {
        return Err(DecodeError::TooLong(b.len()));
    }
    let (v, rest) = postcard::take_from_bytes(b).map_err(|_| DecodeError::Malformed)?;
    if !rest.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(v)
}

fn check(ok: bool, why: &'static str) -> Result<(), DecodeError> {
    if ok { Ok(()) } else { Err(DecodeError::Invalid(why)) }
}

fn sane(f: f32) -> bool {
    f.is_finite() && f.abs() < 1.0e6
}

fn check_snapshot(s: &PlayerSnapshot) -> Result<(), DecodeError> {
    check(s.pos.is_sane() && s.rot.is_sane() && s.aim.is_sane(), "non-finite vector")?;
    check(sane(s.facing2) && sane(s.speed) && sane(s.pod_cooldown), "non-finite float")?;
    check(s.hp <= MAX_HP, "hp out of range")?;
    check(matches!(s.model, 0 | sdk::model::PL_2B | sdk::model::PL_9S | sdk::model::PL_A2), "unknown player model")
}

pub fn decode_datagram(b: &[u8]) -> Result<Datagram, DecodeError> {
    let d: Datagram = decode(b, MAX_DATAGRAM)?;
    match &d {
        Datagram::Player { snap, .. } => check_snapshot(snap)?,
        Datagram::Entities { items, .. } => {
            check(items.len() <= ENTITIES_PER_DATAGRAM, "too many entities")?;
            for e in items {
                check(e.pos.is_sane() && e.rot.is_sane(), "non-finite entity vector")?;
                check(e.hp <= MAX_HP, "entity hp out of range")?;
            }
        }
    }
    Ok(d)
}

fn short_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(|c| c.is_control())
}

pub fn decode_event(b: &[u8]) -> Result<Event, DecodeError> {
    let e: Event = decode(b, MAX_FRAME)?;
    match &e {
        Event::Hello(h) => {
            check(short_text(&h.mod_version, MAX_NAME), "bad version string")?;
            check(short_text(&h.name, MAX_NAME), "bad player name")?;
        }
        Event::Refuse { reason } => check(short_text(reason, MAX_CHAT), "bad refuse reason")?,
        Event::Anim { args, .. } => {
            check(args[0] < MAX_ANIM, "animation id out of range")?;
            check(!sdk::ANIM_CRASHES.contains(&args[0]), "animation id known to crash")?;
        }
        Event::Buttons { .. } | Event::DestroyEntity { .. } | Event::EntityDied { .. } => {}
        Event::EntityAnim { args, .. } => check(args[0] < MAX_ANIM, "entity animation id out of range")?,
        Event::SpawnEntity(s) => {
            check(!s.name.is_empty() && s.name.len() <= MAX_ENTITY_NAME && s.name.bytes().all(|b| b.is_ascii_graphic()), "bad entity name")?;
            check(s.pos.is_sane() && s.rot.is_sane(), "non-finite spawn transform")?;
        }
        Event::Chat { text } => check(short_text(text, MAX_CHAT), "bad chat text")?,
    }
    Ok(e)
}

pub fn encode<T: Serialize>(v: &T) -> Vec<u8> {
    postcard::to_allocvec(v).expect("serializing to a Vec cannot fail")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> PlayerSnapshot {
        PlayerSnapshot {
            time_ms: 1234,
            flags: PlayerSnapshot::FLAG_PL0000,
            pos: cVec4::new(1.0, 2.0, 3.0, 1.0),
            hp: 1000,
            model: sdk::model::PL_2B,
            ..Default::default()
        }
    }

    #[test]
    fn datagram_roundtrip() {
        let d = Datagram::Player { seq: 7, snap: snap() };
        assert_eq!(decode_datagram(&encode(&d)), Ok(d));
        let items = vec![EntityState { net_id: 3, pos: cVec4::default(), rot: cVec4::default(), hp: 5 }; ENTITIES_PER_DATAGRAM];
        let d = Datagram::Entities { seq: 1, time_ms: 2, items };
        let b = encode(&d);
        assert!(b.len() <= MAX_DATAGRAM, "full entity batch is {} bytes", b.len());
        assert_eq!(decode_datagram(&b), Ok(d));
    }

    #[test]
    fn event_roundtrip() {
        for e in [
            Event::Hello(Hello { protocol: 1, mod_version: "0.1.0".into(), game_build: 9, name: "2B".into(), enemy_sync: true }),
            Event::Anim { time_ms: 5, args: [1, 0, 0, 0] },
            Event::Buttons { time_ms: 5, buttons: [1; 8] },
            Event::SpawnEntity(SpawnEntity { net_id: 1, name: "em1000".into(), model: 0x21020, pos: cVec4::default(), rot: cVec4::default() }),
            Event::DestroyEntity { net_id: 1 },
            Event::EntityAnim { net_id: 1, args: [0xAC, 0, 0, 0] },
            Event::EntityDied { net_id: 1 },
            Event::Chat { text: "hello".into() },
        ] {
            assert_eq!(decode_event(&encode(&e)), Ok(e));
        }
    }

    #[test]
    fn rejects_bad_input() {
        let good = encode(&Datagram::Player { seq: 1, snap: snap() });
        assert_eq!(decode_datagram(&good[..good.len() - 1]), Err(DecodeError::Malformed));
        let mut long = good.clone();
        long.push(0);
        assert_eq!(decode_datagram(&long), Err(DecodeError::TrailingBytes));
        assert!(matches!(decode_datagram(&vec![0; MAX_DATAGRAM + 1]), Err(DecodeError::TooLong(_))));
        assert_eq!(decode_datagram(&[0xFF]), Err(DecodeError::Malformed));

        let nan = Datagram::Player { seq: 1, snap: PlayerSnapshot { pos: cVec4::new(f32::NAN, 0.0, 0.0, 0.0), ..snap() } };
        assert!(matches!(decode_datagram(&encode(&nan)), Err(DecodeError::Invalid(_))));
        let huge = Datagram::Player { seq: 1, snap: PlayerSnapshot { speed: 1.0e9, ..snap() } };
        assert!(matches!(decode_datagram(&encode(&huge)), Err(DecodeError::Invalid(_))));
        let model = Datagram::Player { seq: 1, snap: PlayerSnapshot { model: 0xDEAD, ..snap() } };
        assert!(matches!(decode_datagram(&encode(&model)), Err(DecodeError::Invalid(_))));
        let many = Datagram::Entities { seq: 1, time_ms: 0, items: vec![EntityState { net_id: 0, pos: cVec4::default(), rot: cVec4::default(), hp: 0 }; 25] };
        assert!(decode_datagram(&encode(&many)).is_err());

        let crash = Event::Anim { time_ms: 0, args: [27, 0, 0, 0] };
        assert!(matches!(decode_event(&encode(&crash)), Err(DecodeError::Invalid(_))));
        let name = Event::SpawnEntity(SpawnEntity { net_id: 1, name: "a".repeat(32), model: 0, pos: cVec4::default(), rot: cVec4::default() });
        assert!(matches!(decode_event(&encode(&name)), Err(DecodeError::Invalid(_))));
        let anim = Event::EntityAnim { net_id: 1, args: [MAX_ANIM, 0, 0, 0] };
        assert!(matches!(decode_event(&encode(&anim)), Err(DecodeError::Invalid(_))));
        let ctl = Event::Chat { text: "a\nb".into() };
        assert!(matches!(decode_event(&encode(&ctl)), Err(DecodeError::Invalid(_))));
        let long_name = Event::Hello(Hello { protocol: 1, mod_version: "x".into(), game_build: 0, name: "n".repeat(33), enemy_sync: false });
        assert!(matches!(decode_event(&encode(&long_name)), Err(DecodeError::Invalid(_))));
    }
}
