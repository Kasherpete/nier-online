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
    /// Global `CameraGame` object.
    pub camera: usize,
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

    let camera = ctx("camera", (|| {
        let lea = after_string_ref(img, patterns::CAMERA_STRING, 100, "48 8D 0D")?;
        img.insn_target(lea)
    })())?;

    // Every patched site must be safe to overwrite.
    for (name, site) in [
        ("update", update),
        ("spawn", spawn),
        ("held_store", held_store),
        ("pod_no_target", pod_no_target),
        ("start_animation", start_animation),
    ] {
        ctx(&format!("hook site {name}"), img.check_hook_site(site, HOOK_PATCH_LEN))?;
    }

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
        camera,
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
    /// 0x48b992, pod_program 0x529080, Pl0000 vtable 0xc61628, start_animation 0x433c70, camera 0x1020bb0.
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
        for (n, f) in [("update", a.update), ("spawn", a.spawn), ("routine", a.routine), ("pod_program", a.pod_program), ("start_animation", a.start_animation)] {
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
