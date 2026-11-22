//! Reversed engine types. Offsets are for the current Steam build (verified against
//! `Last_SVN_REVISION=7788`, exe dated 2026-11-21).
//!
//! Sources: AutomataMP (MIT) `reversing/nierautomata.genny`, `shared/sdk/*.hpp`; nier_coop `src/sdk/mod.rs`.
//! Every struct here is either the full engine object (size asserted against the engine's allocation size)
//! or an explicitly documented *prefix* that is only ever reached through a pointer, never allocated,
//! copied or indexed as an array.

#![allow(non_camel_case_types)]

pub mod rtti;

use core::mem::offset_of;

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Pod, Zeroable, Serialize, Deserialize)]
pub struct cVec4 {
    pub x: f32, // 0x00
    pub y: f32, // 0x04
    pub z: f32, // 0x08
    pub w: f32, // 0x0C
}
const _: () = assert!(size_of::<cVec4>() == 0x10);

impl cVec4 {
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    pub fn dist(self, o: Self) -> f32 {
        ((self.x - o.x).powi(2) + (self.y - o.y).powi(2) + (self.z - o.z).powi(2)).sqrt()
    }

    pub fn lerp(self, o: Self, t: f32) -> Self {
        Self::new(
            self.x + (o.x - self.x) * t,
            self.y + (o.y - self.y) * t,
            self.z + (o.z - self.z) * t,
            self.w + (o.w - self.w) * t,
        )
    }

    pub fn is_sane(self) -> bool {
        [self.x, self.y, self.z, self.w].iter().all(|f| f.is_finite() && f.abs() < 1.0e6)
    }
}

/// Engine model ids (AutomataMP `EModel`).
pub mod model {
    pub const PL_2B: u32 = 0x10000;
    pub const PL_A2: u32 = 0x10100;
    pub const PL_9S: u32 = 0x10200;
}

/// `BehaviorAppBase` (Model -> Obj -> Behavior -> BehaviorAppBase), the base of every character, enemy and
/// NPC. Full size 0xC50 (AutomataMP `BehaviorAppBase.hpp` static_assert).
#[repr(C)]
pub struct BehaviorAppBase {
    pub vtable: *const usize,      // 0x000
    _pad_008: [u8; 0x48],          // 0x008
    pub position: cVec4,           // 0x050 Model::position
    _pad_060: [u8; 0x30],          // 0x060
    pub rotation: cVec4,           // 0x090 Model rotation; .y (0x94) is Model::facing (yaw)
    _pad_0a0: [u8; 0x4F8],         // 0x0A0
    pub obj_flags: u32,            // 0x598 Obj::obj_flags
    _pad_59c: [u8; 0x1C],          // 0x59C
    pub model_index: u32,          // 0x5B8 Obj::model_index (EModel)
    _pad_5bc: [u8; 0x54],          // 0x5BC
    pub entity: *mut Entity,       // 0x610 Obj::entity (back-pointer to the container)
    _pad_618: [u8; 0x240],         // 0x618
    pub health: u32,               // 0x858
    _pad_85c: [u8; 0x3F4],         // 0x85C
}
const _: () = {
    assert!(size_of::<BehaviorAppBase>() == 0xC50);
    assert!(offset_of!(BehaviorAppBase, position) == 0x50);
    assert!(offset_of!(BehaviorAppBase, rotation) == 0x90);
    assert!(offset_of!(BehaviorAppBase, obj_flags) == 0x598);
    assert!(offset_of!(BehaviorAppBase, model_index) == 0x5B8);
    assert!(offset_of!(BehaviorAppBase, entity) == 0x610);
    assert!(offset_of!(BehaviorAppBase, health) == 0x858);
};

/// Playable character (2B / 9S / A2). Inherits `BehaviorAppBase`; full size 0x17920 (AutomataMP genny,
/// confirmed by nier_coop against the allocation size in the current exe).
#[repr(C)]
pub struct Pl0000 {
    pub base: BehaviorAppBase,       // 0x00000
    _pad_00c50: [u8; 0x190],         // 0x00C50
    pub ai_pod_fire: i32,            // 0x00DE0 > 0: pod fires (read instead of held bit 6 when not player-controlled)
    _pad_00de4: [u8; 0x5F4],         // 0x00DE4
    pub held_flags: u32,             // 0x013D8 CharacterController (@0xCA0) + 0x738; bit i = buttons[i] held
    _pad_013dc: [u8; 0x34],          // 0x013DC
    pub buttons: [u32; 8],           // 0x01410 controller + 0x770: light, heavy, jump, ?, pod special, ?, pod fire, dash
    _pad_01430: [u8; 0x4],           // 0x01430
    pub speed: f32,                  // 0x01434 controller + 0x794: stick magnitude
    _pad_01438: [u8; 0x4],           // 0x01438
    pub facing2: f32,                // 0x0143C controller + 0x79C: stick direction
    _pad_01440: [u8; 0xEF24],        // 0x01440
    pub muteki_timer: f32,           // 0x10364 invincible while > 0 (Pl0000.setEventMuteki sets 300.0)
    _pad_10368: [u8; 0x470],         // 0x10368
    pub weapon_index: u32,           // 0x107D8 (AutomataMP genny)
    _pad_107dc: [u8; 0x44],          // 0x107DC
    pub pod_index: u32,              // 0x10820 active pod (index into pod_cooldown)
    _pad_10824: [u8; 0x1FDC],        // 0x10824
    pub pod_aim: cVec4,              // 0x12800 world point pod_shot aims at
    _pad_12810: [u8; 0x3C6C],        // 0x12810
    pub buddy_handle: u32,           // 0x1647C
    _pad_16480: [u8; 0x8],           // 0x16480
    pub run_speed_type: u32,         // 0x16488 0 = speed from controller, 1 = from buddy AI
    _pad_1648c: [u8; 0x50],          // 0x1648C
    pub pod_target: u32,             // 0x164DC entity handle a non-player pod shoots at; 0 = no shot
    _pad_164e0: [u8; 0x550],         // 0x164E0
    pub pod_cooldown: [[f32; 2]; 3], // 0x16A30 per pod: [full, remaining]; a program starts at remaining <= 0
    _pad_16a48: [u8; 0xA68],         // 0x16A48
    pub flashlight: u8,              // 0x174B0 pod light on (AutomataMP genny)
    _pad_174b1: [u8; 0x46F],         // 0x174B1
}
const _: () = {
    assert!(size_of::<Pl0000>() == 0x17920);
    assert!(offset_of!(Pl0000, ai_pod_fire) == 0xDE0);
    assert!(offset_of!(Pl0000, held_flags) == 0x13D8);
    assert!(offset_of!(Pl0000, buttons) == 0x1410);
    assert!(offset_of!(Pl0000, speed) == 0x1434);
    assert!(offset_of!(Pl0000, facing2) == 0x143C);
    assert!(offset_of!(Pl0000, muteki_timer) == 0x10364);
    assert!(offset_of!(Pl0000, weapon_index) == 0x107D8);
    assert!(offset_of!(Pl0000, pod_index) == 0x10820);
    assert!(offset_of!(Pl0000, pod_aim) == 0x12800);
    assert!(offset_of!(Pl0000, buddy_handle) == 0x1647C);
    assert!(offset_of!(Pl0000, run_speed_type) == 0x16488);
    assert!(offset_of!(Pl0000, pod_target) == 0x164DC);
    assert!(offset_of!(Pl0000, pod_cooldown) == 0x16A30);
    assert!(offset_of!(Pl0000, flashlight) == 0x174B0);
};

/// Offset of `CharacterController` inside `Pl0000`: code inside the controller has `this = pl + 0xCA0`.
pub const CONTROLLER_OFFSET: usize = 0xCA0;
/// `held_flags` bit of the pod-fire button (`buttons[6]`).
pub const HELD_POD_FIRE: u32 = 1 << 6;

/// Gates of the pod-program update for one character (nier_coop, exe 2026-10-09, still valid): the engine
/// runs it only when these `Pl0000` u32 fields are all 0...
pub const POD_PROGRAM_BLOCKERS: [usize; 5] = [0x17678, 0x12A68, 0x12AA4, 0x165FC, 0x12AEC];
/// ...bit 0x20 of this flags field is clear...
pub const POD_PROGRAM_FLAGS: usize = 0x16D18;
/// ...and this state id is not 0x41 or 0x46.
pub const PL_STATE: usize = 0x670;

/// `start_animation` ids that crash the game when started on another character (AutomataMP `EAnimation`).
pub const ANIM_CRASHES: [u32; 4] = [27, 65, 104, 106];
/// `start_animation` ids that the synced controller inputs already cause (light attack, dash). Replaying
/// them as well would start the move twice.
pub const ANIM_FROM_INPUT: [u32; 2] = [16, 18];
/// Animation the spawned partner's buddy AI starts every idle frame; dropped for the remote player.
pub const ANIM_PARTNER_WAIT: u32 = 1_000_000;
/// `EmBase` die states: its die routine (vtable slot 170, what `EmBase.setDieRtn` calls) starts 0xAC, or 0xA9
/// for some models and flags (build 7788, 0x614720). `EmBase::start_animation` remaps 0xAC to 0xAF for a few
/// models *after* our detour sees the call.
pub const EM_DIE_STATES: [u32; 2] = [0xA9, 0xAC];

/// Vtable slot of `Behavior::start_animation(this, anim, variant, a3, a4)` (AutomataMP `Behavior.hpp`).
pub const START_ANIMATION_INDEX: usize = 18;

/// Entity container (prefix; the real object is larger). `behavior` points at the `Behavior` subclass.
#[repr(C)]
pub struct Entity {
    _unknown: usize,            // 0x00
    pub name: [u8; 0x20],       // 0x08 NUL-terminated
    pub flags: u64,             // 0x28
    pub handle: u32,            // 0x30
    _pad_34: u32,               // 0x34
    _unknown2: usize,           // 0x38
    _unknown3: usize,           // 0x40
    pub behavior: *mut BehaviorAppBase, // 0x48 (only a BehaviorAppBase after an RTTI check)
}
const _: () = assert!(size_of::<Entity>() == 0x50 && offset_of!(Entity, behavior) == 0x48);

impl Entity {
    pub fn name_str(&self) -> &str {
        let n = self.name.iter().position(|&b| b == 0).unwrap_or(self.name.len());
        core::str::from_utf8(&self.name[..n]).unwrap_or("?")
    }
}

/// One slot of the global entity array, indexed by `(handle >> 8) & 0xFFFF`.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct EntityDescriptor {
    pub handle: u32,      // 0x00
    _pad_04: u32,         // 0x04
    pub ent: *mut Entity, // 0x08
}
const _: () = assert!(size_of::<EntityDescriptor>() == 0x10);

/// The u32 entity count lives this many bytes before the entity-list global (AutomataMP `EntityList.cpp`).
pub const ENTITY_COUNT_BEFORE_LIST: usize = 0x10;

/// Transform passed to the spawn function (AutomataMP `EntitySpawnParams::PositionalData`).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub struct SpawnMatrix {
    pub forward: cVec4,  // 0x00
    pub up: cVec4,       // 0x10
    pub right: cVec4,    // 0x20
    pub w: cVec4,        // 0x30
    pub position: cVec4, // 0x40
    pub rotation: cVec4, // 0x50 same layout as Model rotation
    pub scale: cVec4,    // 0x60
    pub unk: [u32; 8],   // 0x70 unk[6] (0x88) = 1
}
const _: () = assert!(size_of::<SpawnMatrix>() == 0x90);

impl SpawnMatrix {
    pub fn at(position: cVec4, rotation: cVec4) -> Self {
        let mut unk = [0; 8];
        unk[6] = 1;
        Self {
            forward: cVec4::new(1.0, 0.0, 0.0, 0.0),
            up: cVec4::new(0.0, 1.0, 0.0, 0.0),
            right: cVec4::new(0.0, 0.0, 1.0, 0.0),
            w: cVec4::new(0.0, 0.0, 0.0, 1.0),
            position: cVec4 { w: 1.0, ..position },
            rotation,
            scale: cVec4::new(1.0, 1.0, 1.0, 0.0),
            unk,
        }
    }
}

/// Argument of the entity spawn function (AutomataMP `EntitySpawnParams`).
#[repr(C)]
pub struct SpawnParams {
    pub name: *const u8,            // 0x00 NUL-terminated entity name
    pub model: u32,                 // 0x08 EModel
    pub model2: u32,                // 0x0C same as model
    pub matrix: *const SpawnMatrix, // 0x10
    pub unk1: u64,                  // 0x18
    pub unk2: u32,                  // 0x20 = 1
    _pad_24: u32,                   // 0x24
    pub unk3: u64,                  // 0x28
    pub unk4: cVec4,                // 0x30
    pub unk5: cVec4,                // 0x40
    pub unk6: u64,                  // 0x50
    _pad_58: [u8; 0x28],            // 0x58
}
const _: () = assert!(size_of::<SpawnParams>() == 0x80 && offset_of!(SpawnParams, unk6) == 0x50);

impl SpawnParams {
    /// `name` must be NUL-terminated and outlive the spawn call.
    pub fn new(name: &core::ffi::CStr, model: u32, matrix: &SpawnMatrix) -> Self {
        let z = cVec4::default();
        Self {
            name: name.as_ptr().cast(),
            model,
            model2: model,
            matrix,
            unk1: 0,
            unk2: 1,
            _pad_24: 0,
            unk3: 0,
            unk4: z,
            unk5: z,
            unk6: 0,
            _pad_58: [0; 0x28],
        }
    }
}

/// Node of the engine's circular list of script-callable functions ("Class.method" -> native).
/// AutomataMP `shared/sdk/ScriptFunctions.cpp`.
#[repr(C)]
pub struct ScriptFunction {
    pub vtable: usize,                 // 0x00
    pub crc: u32,                      // 0x08 CRC-32 of "Class.method"
    _pad_0c: u32,                      // 0x0C
    pub next: *const ScriptFunction,   // 0x10
    _unk_18: usize,                    // 0x18
    pub function: usize,               // 0x20
}
const _: () = assert!(size_of::<ScriptFunction>() == 0x28 && offset_of!(ScriptFunction, function) == 0x20);

/// `CameraGame` (prefix; the full object is 0x8C20). AutomataMP genny `CameraApp` / `CameraBase`.
#[repr(C)]
pub struct CameraGame {
    pub vtable: *const usize,  // 0x000
    _pad_008: [u8; 0x58],      // 0x008
    pub data: *const CameraData, // 0x060 CameraBase::data
    _pad_068: [u8; 0xD0],      // 0x068
    pub fov: f32,              // 0x138 CameraApp::fov (radians, vertical)
    _pad_13c: [u8; 0x4],       // 0x13C
}
const _: () = assert!(size_of::<CameraGame>() == 0x140 && offset_of!(CameraGame, fov) == 0x138);

/// Camera matrices (prefix). `view` is the camera's world transform: rows right, up, forward, origin.
#[repr(C)]
pub struct CameraData {
    _pad_00: [u8; 0x80],          // 0x00
    pub viewproj: [[f32; 4]; 4],  // 0x80
    pub view: [[f32; 4]; 4],      // 0xC0
}
const _: () = assert!(size_of::<CameraData>() == 0x100 && offset_of!(CameraData, view) == 0xC0);

// Scene actions (cutscenes, triggers, quest steps). Ported from nier_coop `src/sdk/mod.rs` (GPL-3.0), verified
// there against the 2026-10-09 and 2026-11-19 exes; the action scene layout is re-checked on build 7788 by
// `addr::tests::resolves_against_exe`.

/// One scripted scene action. `state` is the hash of its current state's name.
#[repr(C)]
pub struct SceneAction {
    pub vtable: usize,        // 0x00
    _unknown_08: [u8; 0x10],  // 0x08
    pub id: u32,              // 0x18 layout id (the save's Scene/Action keys); equal on both games
    pub flags: u32,           // 0x1C
    pub state: u32,           // 0x20 state name hash
    _pad_24: u32,             // 0x24
}
const _: () = assert!(size_of::<SceneAction>() == 0x28 && offset_of!(SceneAction, state) == 0x20);

/// A group of scene actions.
#[repr(C)]
pub struct ActionGroup {
    _unknown_00: [u8; 0x30],               // 0x00
    pub actions: *const *mut SceneAction,  // 0x30
    pub count: usize,                      // 0x38
}
const _: () = assert!(size_of::<ActionGroup>() == 0x40 && offset_of!(ActionGroup, count) == 0x38);

/// The static object holding every loaded action group.
#[repr(C)]
pub struct ActionScene {
    _unknown_00: [u8; 0x48],                 // 0x00
    pub groups: *const *const ActionGroup,   // 0x48
    pub count: usize,                        // 0x50
}
const _: () = assert!(size_of::<ActionScene>() == 0x58 && offset_of!(ActionScene, count) == 0x50);

/// Argument of `Action::request_state` (the engine builds it from a state hash).
#[repr(C)]
pub struct StateReq {
    pub hash: u32,           // 0x00 target state name hash
    _pad_04: u32,            // 0x04
    pub name: *const u8,     // 0x08 interned name object (null if the hash has no registered name)
    pub flag: u32,           // 0x10
    _pad_14: u32,            // 0x14
}
const _: () = assert!(size_of::<StateReq>() == 0x18 && offset_of!(StateReq, flag) == 0x10);

impl Default for StateReq {
    fn default() -> Self {
        Self { hash: 0, _pad_04: 0, name: core::ptr::null(), flag: 0, _pad_14: 0 }
    }
}

/// Interned, refcounted engine string (a node of the global string pool). nier_coop `sdk::HashedStr`.
#[repr(C)]
pub struct HashedStr {
    _tree: [usize; 4],   // 0x00 pool tree links
    pub hash: u32,       // 0x20
    _pad_24: u32,        // 0x24
    pub text: *const u8, // 0x28 NUL-terminated (inline at 0x38)
    pub refs: usize,     // 0x30
}
const _: () = assert!(size_of::<HashedStr>() == 0x38 && offset_of!(HashedStr, text) == 0x28);

/// One active scene state ("q120/DONE", "Route/A", ...), kept sorted by name hash. nier_coop `sdk::SceneState`.
#[repr(C)]
pub struct SceneState {
    _unknown_00: [usize; 2],   // 0x00
    pub name: *mut HashedStr,  // 0x10
    _unknown_18: usize,        // 0x18
}
const _: () = assert!(size_of::<SceneState>() == 0x20);

/// `hap::scene_state::SceneStateSystem`, a static object in the image; only its state array is read.
#[repr(C)]
pub struct SceneStateSystem {
    _unknown_00: [u8; 0x98],        // 0x00
    pub states: *const SceneState,  // 0x98
    pub count: usize,               // 0xA0
}
const _: () = assert!(size_of::<SceneStateSystem>() == 0xA8 && offset_of!(SceneStateSystem, count) == 0xA0);

/// Who posted a scene signal: a scene context and the sender action's id. nier_coop `hooks::events::Sender`.
#[repr(C)]
pub struct SignalSender {
    pub context: usize, // 0x00
    pub id: u32,        // 0x08
    _pad_0c: u32,       // 0x0C
}
const _: () = assert!(size_of::<SignalSender>() == 0x10);

impl SignalSender {
    pub fn new(context: usize, id: u32) -> Self {
        Self { context, id, _pad_0c: 0 }
    }
}

/// Scene action state hashes and vtable slots used by world sync (nier_coop `hooks/events.rs`, `sdk::patterns`).
pub mod action {
    /// HapGoalLocationAction: WAIT shows the objective, then its update moves it to ACTIVE.
    pub const WAIT: u32 = 0x4b7b6e51;
    pub const ACTIVE: u32 = 0xbbe63b84;
    pub const DONE: u32 = 0x26b80d1f;
    /// EnemySetArea: WAIT until its area condition holds, SPAWN_DELAY counts down, SPAWN spawns the set.
    pub const SPAWN_DELAY: u32 = 0x45ce422b;
    pub const SPAWN: u32 = 0x3aa89d11;
    /// `SendCommand` slot `run(this, out)`: runs its command list, then requests DONE.
    pub const SEND_COMMAND_RUN: usize = 9;
    /// `HapGoalLocationAction` slot `update(this)`: in WAIT sets the HUD objective and map marker.
    pub const GOAL_UPDATE: usize = 9;
    /// `HapGoalLocationAction` slot `finish(this)`: removes its marker and HUD objective, then requests DONE.
    pub const GOAL_FINISH: usize = 12;
}

/// `PhaseEventAction` (cutscene) state hashes, from the game's state registry (nier_coop `hooks/events.rs`).
pub mod phase {
    /// Unnamed state between LOADED and PLAYING: play was requested.
    pub const PLAY_REQ: u32 = 0xa09a3421;
    pub const PLAYING: u32 = 0xecbccdf8;
    /// Reached the last frames, holding.
    pub const PLAY_END: u32 = 0xcee3b8c9;
    pub const DONE: u32 = 0x26b80d1f;
    /// Started: requested, playing or ending.
    pub const STARTED: [u32; 3] = [PLAY_REQ, PLAYING, PLAY_END];
}

/// `CameraAction` (scripted camera event) state hashes, from a build 7788 session log (2026-11-22): every
/// camera action of an area goes 0 -> WAIT when it loads; the one an enemy wave's intro played went 0 -> PLAY.
pub mod camera {
    pub const WAIT: u32 = 0x4b7b6e51;
    pub const PLAY: u32 = 0x681c330e;
}

/// CRC-32 (IEEE), the hash the engine keys script functions by.
pub fn crc32(s: &[u8]) -> u32 {
    !s.iter().fold(!0u32, |mut c, &b| {
        c ^= b as u32;
        for _ in 0..8 {
            c = (c >> 1) ^ (0xEDB8_8320 & (c & 1).wrapping_neg());
        }
        c
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn crc32_known() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
    }
}
