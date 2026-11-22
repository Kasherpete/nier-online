//! The address registry. Every game address the mod uses is resolved here, once, at startup. If any lookup
//! fails, the error names it and no hooks are installed at all.
//!
//! All values are RVAs (offsets from the module base); add [`Addresses::base`] for an absolute address.
//! Verified against Steam build `Last_SVN_REVISION=7788` (exe PE timestamp in `tests::EXPECTED_BUILD`).

use crate::scan::{Image, Pattern, Result, ScanError, err};
use crate::sdk::{self, rtti};

/// Byte patterns and anchor strings. Sources: AutomataMP `shared/sdk/EntityList.cpp`, `CameraGame.cpp`,
/// `ScriptFunctions.cpp`, `src/mods/multiplayer/MidHooks.cpp`; nier_coop `src/sdk/mod.rs`.
mod patterns {
    /// `shl rcx,4; add rcx,[rip+disp]` -> global `EntityDescriptor*` (operand at +4, instruction 7 bytes).
    pub const ENTITY_LIST: &str = "48 c1 e1 04 48 03 ? ? ? ? ? 8b 01 33 c2 a9 00 ff ff ff 75 0f 48 8b 41 08";
    /// Referenced right before `lea rdx,[possessed_handle]`.
    pub const POSSESSED_STRING: &str = "core_AccelTime_Out";
    pub const LEA_RDX: &str = "48 8D 15";
    /// Middle of the per-frame game update.
    pub const UPDATE_MIDDLE: &str = "? 89 ? e8 00 00 00 ? 89 ? f0 00 00 00 ? ? ? e8 ? ? ? ? ? ? ? ? ? 00 00";
    /// Referenced right before `lea rcx,[spawner]; call spawn`.
    pub const SPAWN_STRING: &str = "MapInstance";
    pub const LEA_RCX_CALL: &str = "48 8D 0D ? ? ? ? E8";
    /// Referenced right before `jmp assign_ai_routine`.
    pub const ROUTINE_STRING: &str = "buddy_pascal";
    /// Referenced right before `mov rax,[script_function_list]`.
    pub const SCRIPT_STRING: &str = "EntityLayout.enableCreateEntity";
    pub const MOV_RAX: &str = "48 8B 05";
    /// Near the end of the controller's button processing.
    pub const BUTTONS_END: &str = "C7 ? 90 07 00 00 14 00 00 00 48";
    /// `mov [rcx+0x740],rax`, right after held_flags is rebuilt from input (rcx = controller).
    pub const HELD_STORE: &str = "48 89 81 40 07 00 00";
    /// pod_shot, non-player branch: `test eax,eax; jne aim_copy; target = entity(pl->pod_target); test rax,rax; je exit`.
    pub const POD_NO_TARGET: &str =
        "85 C0 75 ? 48 8D 97 DC 64 01 00 48 8D 4D B0 E8 ? ? ? ? 48 8B C8 E8 ? ? ? ? 48 85 C0 0F 84";
    /// First bytes at `aim_copy`: `lea rcx,[rdi+0x12800]`.
    pub const AIM_COPY: [u8; 7] = [0x48, 0x8D, 0x8F, 0x00, 0x28, 0x01, 0x00];
    /// Per-character pod-program update (starts/ends pod skills from held bit 4).
    pub const POD_PROGRAM: &str = "4C 8B DC 57 48 81 EC A0 01 00 00 48 8B 05 ? ? ? ? 48 33 C4 48 89 84 24 50 01 00 00 83 B9 60 5B 01 00 00";
    /// Referenced right before `lea rcx,[camera_game]` (AutomataMP `CameraGame.cpp`).
    pub const CAMERA_STRING: &str = "Step : %d / %d";
    /// The getters that turn the possessed handle into the player, which enemy AI asks for its target
    /// (build 7788). Each is found among the references to `possessed`, and its `lea` (at the offset given)
    /// must point there.
    /// `get_player() -> Pl0000*`: copy the handle twice, handle -> entity, entity -> Pl0000. 0x7c4f50.
    pub const GET_PLAYER: (&str, usize) = (
        "48 83 EC 28 48 8D 15 ? ? ? ? 48 8D 4C 24 30 E8 ? ? ? ? 48 8B D0 48 8D 4C 24 38 E8 ? ? ? ? 48 8B C8 E8 ? ? ? ? 48 8B C8 E8 ? ? ? ? 48 83 C4 28 C3",
        4,
    );
    /// Offset of get_player's `call handle_to_entity` (its third call).
    pub const GET_PLAYER_H2E_CALL: usize = 0x25;
    /// `get_player_entity() -> Entity*`: `lea rcx,[possessed]; jmp handle_to_entity`. A leaf without
    /// `.pdata`. 0x7c5020.
    pub const GET_PLAYER_ENTITY: (&str, usize) = ("48 8D 0D ? ? ? ? E9", 0);
    /// `get_player_pos(out) -> out`: the player's position (Model+0x50), or (0, 0, 0, 1) without one. 0x7c5050.
    pub const GET_PLAYER_POS: (&str, usize) = (
        "40 53 48 83 EC 30 48 8B D9 48 8D 15 ? ? ? ? 48 8D 4C 24 48 E8 ? ? ? ? 48 8B D0 48 8D 4C 24 50 E8 ? ? ? ? 48 8B C8 E8 ? ? ? ? 48 8B C8 E8 ? ? ? ? 48 8B CB 48 85 C0 75 ? F3 0F 10 05 ? ? ? ? 0F 57 DB 0F 57 D2 F3 0F 11 44 24 20 0F 57 C9 E8 ? ? ? ? 48 8B C3 48 83 C4 30 5B C3 48 8D 50 50 E8",
        9,
    );
    /// Start of `Pl0000::warp(this, &pos, &rot, 0, 0.0)`, the engine's teleport (it moves the previous
    /// positions and the collision body along with `position`). nier_coop `sdk::patterns::WARP` (exe 2026-10-09:
    /// 0x4f0fa0).
    pub const WARP: &str = "48 8B C4 55 56 57 41 54 41 55 48 8D 68 A9 48 81 EC A0 00 00 00 81 B9 70 06 00 00 D3 00 00 00";
    /// The kill-area death in the player's hit update (nier_coop `notes/death-and-warp.md`):
    /// `mov [r12+0x10668],esi; mov [r12+0x858],esi` (life = health = 0), then `lea edx,[r9+0x35]` (death state).
    pub const KILL_AREA: &str = "41 89 B4 24 68 06 01 00 41 89 B4 24 58 08 00 00";
    /// First bytes of `Action::request_state(this, &StateReq)` (nier_coop `sdk::patterns::REQUEST_STATE`).
    pub const REQUEST_STATE: &str = "48 89 5C 24 10 57 48 83 EC 30 48 8B D9 48 8B FA 8B 0A 39 4B 20";
    /// Offset of the `call request_state` inside the `PhaseEventAction.load` native.
    pub const LOAD_REQUEST_CALL: usize = 0x8C;
    /// `lea rcx,[action_scene]` at +0x14 (nier_coop `sdk::patterns::ACTION_SCENE`).
    pub const ACTION_SCENE: &str = "8B 05 ? ? ? ? 0F BA F0 1C 0F BA E8 1E 89 05 ? ? ? ? 48 8D 0D ? ? ? ? E8";
    pub const ACTION_SCENE_LEA: usize = 0x14;
}

/// Native behind a script function, read from its static registration: the initializer that references
/// the name loads the native with `lea rax,[native]` within its first four instructions.
fn script_native(img: &Image, name: &str) -> Result<usize> {
    let lea = pat("48 8D 05");
    let mut found: Vec<usize> = img
        .string_refs(name)?
        .into_iter()
        .filter_map(|r| {
            let init = img.function_at(r).ok()?.begin;
            img.insn_target(img.find_insn(init, 4, &lea).ok()?).ok()
        })
        .filter(|&f| img.is_code(f))
        .collect();
    found.dedup();
    match found[..] {
        [one] => Ok(one),
        [] => err("no registration found"),
        _ => err(format!("{} different natives", found.len())),
    }
}

/// Bytes a hook overwrites (ilhook always writes a 14-byte absolute jmp; retour writes at most that).
pub const HOOK_PATCH_LEN: usize = 14;

#[derive(Debug, Clone)]
pub struct Addresses {
    /// Runtime module base (0 for an offline image).
    pub base: usize,
    /// PE timestamp of the exe: identifies the game build in the handshake.
    pub build: u32,
    /// Global `EntityDescriptor*`; the u32 count is `ENTITY_COUNT_BEFORE_LIST` bytes before it.
    pub entity_list: usize,
    /// Global u32 handle of the entity the player controls.
    pub possessed: usize,
    /// Per-frame game update function (hooked at entry; signature unknown).
    pub update: usize,
    /// `fn(spawner, &SpawnParams) -> *mut Entity`.
    pub spawn: usize,
    /// The spawner object passed as `this` to `spawn`.
    pub spawner: usize,
    /// `fn(name, *mut Entity)`: assigns an AI routine.
    pub routine: usize,
    /// Global `ScriptFunction*` (head of the circular list).
    pub scripts: usize,
    /// Mid-function hook site in the controller's button processing (rcx = controller).
    pub held_store: usize,
    /// Mid-function hook site in pod_shot (`test rax,rax` after the target lookup).
    pub pod_no_target: usize,
    /// Where pod_shot continues when it has an aim point.
    pub pod_aim_copy: usize,
    /// `fn(*mut Pl0000)`: per-character pod-program update.
    pub pod_program: usize,
    /// Primary `Pl0000` vtable.
    pub pl0000_vtable: usize,
    /// `Pl0000::start_animation` (vtable slot 18).
    pub start_animation: usize,
    /// Primary `EmBase` vtable (every enemy class derives from it).
    pub em_base_vtable: usize,
    /// `EmBase::start_animation` (vtable slot 18), shared by all 34 enemy classes on build 7788: each one's
    /// slot 18 is this function, a `jmp` to it, or a wrapper that calls it.
    pub em_start_animation: usize,
    /// Global `CameraGame` object.
    pub camera: usize,
    /// `fn() -> *mut Pl0000`: the possessed character (null if it is not a Pl0000).
    pub get_player: usize,
    /// `fn() -> *mut Entity`: the possessed entity.
    pub get_player_entity: usize,
    /// `fn(out: *mut cVec4) -> *mut cVec4`: the possessed character's position.
    pub get_player_pos: usize,
    /// Primary vtables of every class deriving from `EmBase` (enemies and bosses).
    pub em_vtables: Vec<usize>,
    /// `fn(*mut Pl0000, *const cVec4 pos, *const cVec4 rot, u32 0, f32 0.0)`: the engine's teleport.
    pub warp: usize,
    /// `fn(*mut SceneAction, *const StateReq) -> bool`: every scene action's state change (hooked).
    pub request_state: usize,
    /// `fn(*mut SceneAction) -> bool`: the `PhaseEventAction.play` native (loads the cutscene if needed).
    pub phase_play: usize,
    /// Static `ActionScene`: every loaded scene action group.
    pub action_scene: usize,
    /// Primary `hap::PhaseEventAction` vtable (cutscenes).
    pub phase_vtable: usize,
}

impl Addresses {
    pub fn abs(&self, rva: usize) -> usize {
        self.base + rva
    }
}

fn ctx<T>(what: &str, r: Result<T>) -> Result<T> {
    r.map_err(|e| ScanError(format!("{what}: {e}")))
}

fn pat(s: &str) -> Pattern {
    Pattern::parse(s).expect("built-in pattern is valid")
}

/// First match of `p` within `n` instructions after any reference to `string`.
fn after_string_ref(img: &Image, string: &str, n: usize, p: &str) -> Result<usize> {
    let p = pat(p);
    let refs = img.string_refs(string)?;
    refs.iter()
        .find_map(|&r| img.find_insn(r, n, &p).ok())
        .ok_or(ScanError(format!("no {p:02x?} after any of {} references", refs.len())))
}

pub fn resolve(img: &Image, base: usize) -> Result<Addresses> {
    let entity_list = ctx("entity_list", (|| {
        let at = img.find_unique_code("pattern", &pat(patterns::ENTITY_LIST))?;
        let list = img.insn_target(at + 4)?;
        if list < sdk::ENTITY_COUNT_BEFORE_LIST {
            return err("count field outside the image");
        }
        Ok(list)
    })())?;

    let possessed = ctx("possessed", (|| {
        let lea = after_string_ref(img, patterns::POSSESSED_STRING, 10, patterns::LEA_RDX)?;
        img.insn_target(lea)
    })())?;

    let update = ctx("update", (|| {
        let mid = img.find_unique_code("pattern", &pat(patterns::UPDATE_MIDDLE))?;
        Ok(img.function_at(mid)?.begin)
    })())?;

    let (spawner, spawn) = ctx("spawn", (|| {
        let lea = after_string_ref(img, patterns::SPAWN_STRING, 30, patterns::LEA_RCX_CALL)?;
        Ok((img.insn_target(lea)?, img.insn_target(lea + 7)?))
    })())?;

    let routine = ctx("routine", (|| {
        let jmp = after_string_ref(img, patterns::ROUTINE_STRING, 4, "E9")?;
        img.insn_target(jmp)
    })())?;

    let scripts = ctx("scripts", (|| {
        let mov = after_string_ref(img, patterns::SCRIPT_STRING, 10, patterns::MOV_RAX)?;
        img.insn_target(mov)
    })())?;

    let held_store = ctx("held_store", (|| {
        let end = img.find_unique_code("pattern", &pat(patterns::BUTTONS_END))?;
        let func = img.function_at(end)?.begin;
        img.find_insn(func, 0x100, &pat(patterns::HELD_STORE))
    })())?;

    let (pod_no_target, pod_aim_copy) = ctx("pod_no_target", (|| {
        let at = img.find_unique_code("pattern", &pat(patterns::POD_NO_TARGET))?;
        let aim_copy = img.insn_target(at + 2)?; // the `jne rel8`
        if img.bytes.get(aim_copy..aim_copy + 7) != Some(&patterns::AIM_COPY[..]) {
            return err(format!("unexpected code at aim copy {aim_copy:#x}"));
        }
        Ok((at + 0x1C, aim_copy))
    })())?;

    let pod_program = ctx("pod_program", img.find_unique_code("pattern", &pat(patterns::POD_PROGRAM)))?;

    let pl0000_vtable = ctx("Pl0000 vtable", rtti::find_vtable(img, "Pl0000"))?;
    let start_animation =
        ctx("start_animation", rtti::vtable_slot(img, pl0000_vtable, sdk::START_ANIMATION_INDEX))?;
    let em_base_vtable = ctx("EmBase vtable", rtti::find_vtable(img, "EmBase"))?;
    let em_start_animation =
        ctx("em_start_animation", rtti::vtable_slot(img, em_base_vtable, sdk::START_ANIMATION_INDEX))?;

    let camera = ctx("camera", (|| {
        let lea = after_string_ref(img, patterns::CAMERA_STRING, 100, "48 8D 0D")?;
        img.insn_target(lea)
    })())?;

    let find_getter = |(p, lea_at): (&str, usize), extra: &dyn Fn(usize) -> bool| -> Result<usize> {
        let p = pat(p);
        let mut hits: Vec<usize> = img
            .refs_to(possessed)
            .into_iter()
            .filter_map(|r| (r.checked_sub(7 + lea_at)).filter(|&s| p.matches_at(img.bytes, s)))
            .filter(|&s| img.insn_target(s + lea_at).ok() == Some(possessed) && extra(s))
            .collect();
        hits.dedup();
        match hits[..] {
            [one] => Ok(one),
            [] => err("no match"),
            _ => err(format!("{} matches", hits.len())),
        }
    };
    let get_player = ctx("get_player", find_getter(patterns::GET_PLAYER, &|_| true))?;
    // get_player's third call is handle -> entity. get_player_entity tail-jumps to the same function (a
    // static initializer elsewhere also starts with `lea rcx,[possessed]; jmp`, to something else).
    let handle_to_entity = ctx("handle_to_entity", img.insn_target(get_player + patterns::GET_PLAYER_H2E_CALL))?;
    let get_player_entity = ctx(
        "get_player_entity",
        find_getter(patterns::GET_PLAYER_ENTITY, &|s| img.insn_target(s + 7).ok() == Some(handle_to_entity)),
    )?;
    let get_player_pos = ctx("get_player_pos", find_getter(patterns::GET_PLAYER_POS, &|_| true))?;

    let em_vtables = ctx("EmBase vtables", (|| {
        let v: Vec<usize> =
            rtti::all_vtables(img).into_iter().filter(|(_, ch)| ch.iter().any(|c| c == ".?AVEmBase@@")).map(|(vt, _)| vt).collect();
        if v.len() < 30 {
            return err(format!("only {} classes derive from EmBase", v.len()));
        }
        for &vt in &v {
            for slot in sdk::UPDATE_SLOTS {
                rtti::vtable_slot(img, vt, slot)?;
            }
        }
        Ok(v)
    })())?;

    let warp = ctx("warp", img.find_unique_code("pattern", &pat(patterns::WARP)))?;
    // Shared death writes `life` and starts the death state the way a kill area does; check that code is
    // still there with the same offsets.
    ctx("kill area", img.find_unique_code("pattern", &pat(patterns::KILL_AREA)))?;
    let request_state = ctx("request_state", (|| {
        let load = script_native(img, "PhaseEventAction.load")?;
        let call = load + patterns::LOAD_REQUEST_CALL;
        if img.bytes.get(call) != Some(&0xE8) {
            return err(format!("no call at PhaseEventAction.load+{:#x}", patterns::LOAD_REQUEST_CALL));
        }
        let f = img.insn_target(call)?;
        if !pat(patterns::REQUEST_STATE).matches_at(img.bytes, f) {
            return err(format!("unexpected code at {f:#x}"));
        }
        Ok(f)
    })())?;
    let phase_play = ctx("PhaseEventAction.play", script_native(img, "PhaseEventAction.play"))?;
    let action_scene = ctx("action_scene", (|| {
        let at = img.find_unique_code("pattern", &pat(patterns::ACTION_SCENE))?;
        img.insn_target(at + patterns::ACTION_SCENE_LEA)
    })())?;
    let phase_vtable = ctx("PhaseEventAction vtable", rtti::find_vtable(img, "PhaseEventAction@hap"))?;

    // Every patched site must be safe to overwrite.
    for (name, site) in [
        ("request_state", request_state),
        ("get_player", get_player),
        ("get_player_pos", get_player_pos),
        ("update", update),
        ("spawn", spawn),
        ("held_store", held_store),
        ("pod_no_target", pod_no_target),
        ("start_animation", start_animation),
        ("em_start_animation", em_start_animation),
    ] {
        ctx(&format!("hook site {name}"), img.check_hook_site(site, HOOK_PATCH_LEN))?;
    }
    // get_player_entity has no `.pdata`, so `check_hook_site` can't look at it; its whole body is the 12
    // bytes the pattern matched (`lea; jmp`), no branch can land inside them except at the start, and the
    // retour patch (a 5-byte jmp) stays inside the `lea`.

    Ok(Addresses {
        base,
        build: img.timestamp,
        entity_list,
        possessed,
        update,
        spawn,
        spawner,
        routine,
        scripts,
        held_store,
        pod_no_target,
        pod_aim_copy,
        pod_program,
        pl0000_vtable,
        start_animation,
        em_base_vtable,
        em_start_animation,
        camera,
        get_player,
        get_player_entity,
        get_player_pos,
        em_vtables,
        warp,
        request_state,
        phase_play,
        action_scene,
        phase_vtable,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Reads and maps the exe named by `NIER_EXE`.
    pub fn exe_image() -> Vec<u8> {
        let path = std::env::var("NIER_EXE").expect("set NIER_EXE to the path of NieRAutomata.exe");
        crate::scan::map_file(&std::fs::read(path).expect("read NIER_EXE")).expect("map exe")
    }

    /// Exe these RVAs were verified against: Steam `Last_SVN_REVISION=7788`, file dated 2026-11-21.
    /// Resolved there: entity_list 0x1029860, possessed 0x12505dc, update 0x7b4d00, spawn 0x745af0, spawner
    /// 0x1029898, routine 0x3ec540, scripts 0xfc9888, held_store 0x52a7aa, pod_no_target 0x48b943, aim copy
    /// 0x48b992, pod_program 0x529080, Pl0000 vtable 0xc61628, start_animation 0x433c70, camera 0x1020bb0,
    /// EmBase vtable 0xc79d78, em_start_animation 0x637970 (added 2026-11-22, same exe).
    /// The first nine match what nier_coop resolved at runtime on the same exe (its log, module base
    /// 0x7ff661fc0000).
    const EXPECTED_BUILD: u32 = 0x6ac8_9519;

    /// `NIER_EXE=<exe> cargo test resolves_against_exe -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn resolves_against_exe() {
        let bytes = exe_image();
        let img = Image::new(&bytes, None).unwrap();
        let a = resolve(&img, 0).unwrap();
        println!("{a:#x?}");
        if EXPECTED_BUILD != 0 && a.build != EXPECTED_BUILD {
            println!("note: exe build {:#x} differs from the verified build {EXPECTED_BUILD:#x}", a.build);
        }
        // Function entries must be real function starts.
        for (n, f) in [
            ("update", a.update),
            ("spawn", a.spawn),
            ("routine", a.routine),
            ("pod_program", a.pod_program),
            ("start_animation", a.start_animation),
            ("em_start_animation", a.em_start_animation),
        ] {
            assert_eq!(img.function_at(f).unwrap().begin, f, "{n} is not a function start");
        }
        // Mid-function sites are what the hooks expect.
        assert_eq!(&img.bytes[a.held_store..a.held_store + 7], &[0x48, 0x89, 0x81, 0x40, 0x07, 0x00, 0x00]);
        assert_eq!(&img.bytes[a.pod_no_target..a.pod_no_target + 5], &[0x48, 0x85, 0xC0, 0x0F, 0x84]);
        // The entity list global and the possessed handle live in writable data, not code.
        assert!(!img.is_code(a.entity_list) && !img.is_code(a.possessed) && !img.is_code(a.scripts));
        let classes = rtti::class_hierarchy(&img, a.pl0000_vtable).unwrap();
        println!("Pl0000 bases: {classes:?}");
        assert!(classes.iter().any(|c| c == ".?AVBehaviorAppBase@@"));
        let em = rtti::class_hierarchy(&img, a.em_base_vtable).unwrap();
        assert_eq!(em.first().map(String::as_str), Some(".?AVEmBase@@"));
        assert!(em.iter().any(|c| c == ".?AVBehaviorAppBase@@"));
        // Same signature as Pl0000's: it reads the current state at +0x670 (BehaviorAppBase) early on.
        assert_ne!(a.em_start_animation, a.start_animation);
    }

    /// Every enemy class's `start_animation` slot reaches `em_start_animation`: directly, through a `jmp`
    /// thunk, or through a wrapper that calls it. If a new enemy class overrides it, its animations would not
    /// be synced, so this lists them.
    /// `NIER_EXE=<exe> cargo test enemy_classes_share_start_animation -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn enemy_classes_share_start_animation() {
        let bytes = exe_image();
        let img = Image::new(&bytes, None).unwrap();
        let a = resolve(&img, 0).unwrap();
        let target = a.em_start_animation;
        let call_or_jmp = |at: usize| {
            img.walk(at, 16, |ip, ins| {
                matches!(ins.mnemonic(), iced_x86::Mnemonic::Call | iced_x86::Mnemonic::Jmp) && img.insn_target(ip).ok() == Some(target)
            })
            .is_some()
        };
        let mut n = 0;
        for (vt, chain) in rtti::all_vtables(&img) {
            if !chain.iter().any(|c| c == ".?AVEmBase@@") {
                continue;
            }
            n += 1;
            let f = rtti::vtable_slot(&img, vt, sdk::START_ANIMATION_INDEX).unwrap();
            assert!(f == target || call_or_jmp(f), "{} overrides start_animation ({f:#x})", chain[0]);
        }
        println!("{n} EmBase classes share em_start_animation");
        assert!(n >= 30, "only {n} EmBase classes found");
    }

    /// The `EmBase` script natives enemy sync calls (looked up at runtime by CRC in the script table) still do
    /// what `sync::world::set_no_die` / `die_routine` assume. Each native is registered by a static initializer
    /// that loads it with `lea rax,[native]` right before `lea rcx,[name]`. Verified on build 7788.
    /// `NIER_EXE=<exe> cargo test enemy_natives -- --ignored`
    #[test]
    #[ignore]
    fn enemy_natives() {
        let bytes = exe_image();
        let img = Image::new(&bytes, None).unwrap();
        let native = |name: &str| {
            let r = img.string_refs(name).unwrap()[0];
            let init = img.function_at(r).unwrap().begin;
            img.insn_target(img.find_insn(init, 4, &pat("48 8D 05")).unwrap()).unwrap()
        };
        let code = |at: usize, n: usize| &img.bytes[at..at + n];
        // mov dword ptr [rcx+2C330h],1 / 0; ret
        assert_eq!(code(native("EmBase.onNoDieSuperArmor"), 11), [0xC7, 0x81, 0x30, 0xC3, 0x02, 0, 1, 0, 0, 0, 0xC3]);
        assert_eq!(code(native("EmBase.offNoDieSuperArmor"), 11), [0xC7, 0x81, 0x30, 0xC3, 0x02, 0, 0, 0, 0, 0, 0xC3]);
        // mov rax,[rcx]; jmp qword ptr [rax+550h]  (vtable slot 170)
        assert_eq!(code(native("EmBase.setDieRtn"), 9), [0x48, 0x8B, 0x01, 0xFF, 0xA0, 0x50, 0x05, 0, 0]);
        // EmBase's slot 170 starts a die state: `mov edx,0ACh` (or 0A9h) before calling start_animation.
        let a = resolve(&img, 0).unwrap();
        let die = rtti::vtable_slot(&img, a.em_base_vtable, 170).unwrap();
        let die_end = img.function_at(die).unwrap().end;
        let starts = |id: u32| Pattern::exact(&[&[0xBA][..], &id.to_le_bytes()].concat()).find_all(&img.bytes[die..die_end]).count();
        assert!(sdk::EM_DIE_STATES.iter().all(|&s| starts(s) > 0), "die routine no longer starts {:x?}", sdk::EM_DIE_STATES);
    }

    /// The player getters enemy targeting hooks (`sync::aggro`) and the enemy update slots it wraps.
    /// Verified on build 7788 (2026-11-22): get_player 0x7c4f50, get_player_entity 0x7c5020, get_player_pos
    /// 0x7c5050, 34+ EmBase classes, EmBase slot 7 = 0x6009f0 and slot 8 = 0x688dd0.
    /// `NIER_EXE=<exe> cargo test player_getters_and_enemy_updates -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn player_getters_and_enemy_updates() {
        let bytes = exe_image();
        let img = Image::new(&bytes, None).unwrap();
        let t = std::time::Instant::now();
        let a = resolve(&img, 0).unwrap();
        println!("resolve took {:?}; {} EmBase classes", t.elapsed(), a.em_vtables.len());
        println!("get_player {:#x}, get_player_entity {:#x}, get_player_pos {:#x}", a.get_player, a.get_player_entity, a.get_player_pos);
        if a.build == EXPECTED_BUILD {
            assert_eq!((a.get_player, a.get_player_entity, a.get_player_pos), (0x7c4f50, 0x7c5020, 0x7c5050));
            let em = |s| rtti::vtable_slot(&img, a.em_base_vtable, s).unwrap();
            assert_eq!((em(7), em(8)), (0x6009f0, 0x688dd0));
        }
        assert!(a.em_vtables.contains(&a.em_base_vtable));
        assert!(a.em_vtables.len() >= 34, "only {} EmBase classes", a.em_vtables.len());
        for f in [a.get_player, a.get_player_pos] {
            assert_eq!(img.function_at(f).unwrap().begin, f);
        }
        // get_player_entity: `lea rcx,[possessed]; jmp handle_to_entity`, and get_player calls the same
        // handle_to_entity (its third call), so both return the same entity.
        let h2e = img.insn_target(a.get_player_entity + 7).unwrap();
        assert_eq!(img.insn_target(a.get_player + patterns::GET_PLAYER_H2E_CALL).unwrap(), h2e);
        // Every enemy class's update slots point at code, and none of them is one of our other hook targets.
        for &vt in &a.em_vtables {
            for s in sdk::UPDATE_SLOTS {
                let f = rtti::vtable_slot(&img, vt, s).unwrap();
                assert!(img.is_code(f) && f != a.em_start_animation);
            }
        }
    }

    /// Shared death and cutscene sync (`sync::events`): the warp, the kill-area code whose offsets shared
    /// death copies, `request_state`, `PhaseEventAction.play`, the action scene and the cutscene vtable.
    /// `NIER_EXE=<exe> cargo test death_and_cutscene -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn death_and_cutscene() {
        let bytes = exe_image();
        let img = Image::new(&bytes, None).unwrap();
        let a = resolve(&img, 0).unwrap();
        println!(
            "warp {:#x}, request_state {:#x}, phase_play {:#x}, action_scene {:#x}, phase_vtable {:#x}",
            a.warp, a.request_state, a.phase_play, a.action_scene, a.phase_vtable
        );
        for f in [a.warp, a.request_state, a.phase_play] {
            assert_eq!(img.function_at(f).unwrap().begin, f, "{f:#x} is not a function start");
        }
        assert!(!img.is_code(a.action_scene));
        let chain = rtti::class_hierarchy(&img, a.phase_vtable).unwrap();
        assert_eq!(chain.first().map(String::as_str), Some(".?AVPhaseEventAction@hap@@"));
        // The kill area's death state follows its two stores: `lea edx,[r9+0x35]`.
        let k = img.find_unique_code("pattern", &pat(patterns::KILL_AREA)).unwrap();
        let near = &img.bytes[k..k + 0x40];
        assert!(near.windows(4).any(|w| w == [0x41, 0x8D, 0x51, sdk::ANIM_DEATH as u8]), "no death state after the kill area");
        // The warp refuses state 0xD3 (`cmp dword ptr [rcx+0x670],0xD3` in its prologue).
        assert_eq!(&img.bytes[a.warp + 0x15..a.warp + 0x17], &[0x81, 0xB9]);
        if a.build == EXPECTED_BUILD {
            assert_eq!(
                (a.warp, a.request_state, a.phase_play, a.action_scene, a.phase_vtable),
                (0x4f0fa0, 0x3ec650, 0x3a91f0, 0xfca888, 0xc40758), // verified 2026-11-22
                "build 7788 values changed"
            );
        }
    }

    /// Save redirection (`hooks::savefile`) relies on every save file being opened by name, from one path
    /// builder, through `CreateFileA` / `DeleteFileA` (`notes/2026-11-22-save-files-reversing.md`). Verified on
    /// build 7788: the builder is 0x282cd0, with 4 callers.
    /// `NIER_EXE=<exe> cargo test save_file_io -- --ignored`
    #[test]
    #[ignore]
    fn save_file_io() {
        let bytes = exe_image();
        let img = Image::new(&bytes, None).unwrap();
        let refs = img.string_refs("%sSlotData_%d.dat").unwrap();
        assert_eq!(refs.len(), 1, "slot file name built in more than one place");
        let builder = img.function_at(refs[0]).unwrap().begin;
        assert!(img.refs_to(builder).len() >= 4, "save path builder has fewer callers than read/read/write/delete");
        if img.timestamp == EXPECTED_BUILD {
            assert_eq!(builder, 0x282cd0);
        }
        for name in ["CreateFileA", "DeleteFileA", "ReadFile", "WriteFile"] {
            assert!(img.find_string(name).is_ok(), "{name} no longer imported");
        }
    }

    /// Prints which DLLs the exe imports (is dinput8 a static import?) and other build facts.
    #[test]
    #[ignore]
    fn exe_facts() {
        let bytes = exe_image();
        let img = Image::new(&bytes, None).unwrap();
        println!("timestamp {:#x}, base {:#x}, {} functions", img.timestamp, img.va_base, img.functions.len());
        for s in &img.sections {
            println!("section {:?} rva {:#x} size {:#x} code {}", String::from_utf8_lossy(&s.name), s.rva, s.size, s.is_code());
        }
        let pe = img.u32_at(0x3C).unwrap() as usize;
        let mut d = img.u32_at(pe + 24 + 120).unwrap() as usize;
        loop {
            let name = img.u32_at(d + 12).unwrap() as usize;
            if name == 0 {
                break;
            }
            let end = img.bytes[name..].iter().position(|&b| b == 0).unwrap();
            println!("import {}", String::from_utf8_lossy(&img.bytes[name..name + end]));
            d += 20;
        }
    }
}
