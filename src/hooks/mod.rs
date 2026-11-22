//! Hook installation. Hooks only copy data in or out and return; networking never runs inside a hook
//! (CLAUDE.md architecture rule 2). The sync logic they call lives in `crate::sync`.
//!
//! - Mid-function and entry hooks use `ilhook`, through a shim that also preserves xmm4/xmm5 (ilhook saves
//!   only xmm0-3, but xmm4/5 can be live in the middle of a function) and gives the callback a full
//!   0x20-byte shadow area.
//! - Function detours with a known signature use `retour::GenericDetour`.
//!
//! Every site was checked by `scan::Image::check_hook_site` in `addr::resolve` before we get here.

pub mod savefile;

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use ilhook::x64::{CallbackOption, HookFlags, HookType, Hooker, Registers};
use retour::GenericDetour;
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};

use crate::addr::Addresses;
use crate::scan::Image;
use crate::sdk::{self, Entity, SceneAction, SpawnParams, StateReq, cVec4, rtti};

/// Resolved addresses plus the live module image (for RTTI checks and module-range checks).
pub struct Game {
    pub addr: Addresses,
    pub img: Image<'static>,
}

static GAME: OnceLock<Game> = OnceLock::new();
/// Module base, for the crash report (readable without `game()`, which may not be set yet).
pub static GAME_BASE: AtomicUsize = AtomicUsize::new(0);

/// Set once before any hook is installed, so hooks can always unwrap it.
pub fn game() -> &'static Game {
    GAME.get().expect("hooks run only after install")
}

impl Game {
    /// True if `addr` lies inside the game module.
    pub fn in_module(&self, addr: usize) -> bool {
        let base = self.img.bytes.as_ptr() as usize;
        addr >= base && addr < base + self.img.bytes.len()
    }
}

// -------------------------------------------------------------------------------------------------------
// ilhook shims

type MidFn = fn(&mut Registers);
type RetFn = fn(&mut Registers, usize) -> usize;

/// ilhook calls this as a `JmpBackRoutine(regs, user_data)`; it saves xmm4/5 and calls `mid_dispatch`.
#[unsafe(naked)]
unsafe extern "win64" fn mid_shim(_regs: *mut Registers, _user: usize) {
    core::arch::naked_asm!(
        "push rbx",
        "mov rbx, rsp",
        "and rsp, -16",
        "sub rsp, 0x40",
        "movaps [rsp+0x20], xmm4",
        "movaps [rsp+0x30], xmm5",
        "call {f}",
        "movaps xmm4, [rsp+0x20]",
        "movaps xmm5, [rsp+0x30]",
        "mov rsp, rbx",
        "pop rbx",
        "ret",
        f = sym mid_dispatch,
    )
}

extern "win64" fn mid_dispatch(regs: *mut Registers, user: usize) {
    // SAFETY: `user` is the `MidFn` passed to `mid_hook`; `regs` is ilhook's saved-register block.
    let f: MidFn = unsafe { std::mem::transmute(user) };
    if let Some(r) = unsafe { regs.as_mut() } {
        f(r);
    }
}

/// ilhook calls this as a `JmpToRetRoutine(regs, original, user_data)`; same saving as `mid_shim`.
#[unsafe(naked)]
unsafe extern "win64" fn ret_shim(_regs: *mut Registers, _ori: usize, _user: usize) -> usize {
    core::arch::naked_asm!(
        "push rbx",
        "mov rbx, rsp",
        "and rsp, -16",
        "sub rsp, 0x40",
        "movaps [rsp+0x20], xmm4",
        "movaps [rsp+0x30], xmm5",
        "call {f}",
        "movaps xmm4, [rsp+0x20]",
        "movaps xmm5, [rsp+0x30]",
        "mov rsp, rbx",
        "pop rbx",
        "ret",
        f = sym ret_dispatch,
    )
}

extern "win64" fn ret_dispatch(regs: *mut Registers, ori: usize, user: usize) -> usize {
    // SAFETY: as in `mid_dispatch`.
    let f: RetFn = unsafe { std::mem::transmute(user) };
    match unsafe { regs.as_mut() } {
        Some(r) => f(r, ori),
        None => ori,
    }
}

/// Runs `f` with the registers every time execution reaches `site`, then continues there.
fn mid_hook(name: &str, site: usize, f: MidFn) -> Result<(), String> {
    let h = Hooker::new(site, HookType::JmpBack(mid_shim), CallbackOption::None, f as usize, HookFlags::empty());
    // SAFETY: `site` is an instruction boundary checked by `check_hook_site`.
    let p = unsafe { h.hook() }.map_err(|e| format!("{name} hook at {site:#x}: {e:?}"))?;
    std::mem::forget(p); // never unhooked; the DLL is never unloaded
    tracing::info!("hooked {name} at {site:#x}");
    Ok(())
}

/// Like `mid_hook`, but `f` returns where to continue: the `original` it was given (the relocated original
/// instructions) or another address.
fn ret_hook(name: &str, site: usize, f: RetFn) -> Result<(), String> {
    let h = Hooker::new(site, HookType::JmpToRet(ret_shim), CallbackOption::None, f as usize, HookFlags::empty());
    // SAFETY: as in `mid_hook`.
    let p = unsafe { h.hook() }.map_err(|e| format!("{name} hook at {site:#x}: {e:?}"))?;
    std::mem::forget(p);
    tracing::info!("hooked {name} at {site:#x}");
    Ok(())
}

// -------------------------------------------------------------------------------------------------------
// Detours

pub type SpawnFn = unsafe extern "system" fn(*mut c_void, *const SpawnParams) -> *mut Entity;
pub type StartAnimFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32);
pub type GetPtrFn = unsafe extern "system" fn() -> *mut c_void;
pub type GetPosFn = unsafe extern "system" fn(*mut cVec4) -> *mut cVec4;
/// `Action::request_state(this, &StateReq) -> bool` (the bool is in al; rax is passed through whole).
pub type RequestStateFn = unsafe extern "system" fn(*mut SceneAction, *const StateReq) -> usize;
/// Behavior update slots take only `this`; the other argument registers are passed through untouched.
type UpdateFn = unsafe extern "system" fn(*mut c_void, usize, usize, usize) -> usize;

pub static SPAWN: OnceLock<GenericDetour<SpawnFn>> = OnceLock::new();
pub static START_ANIM: OnceLock<GenericDetour<StartAnimFn>> = OnceLock::new();
pub static EM_START_ANIM: OnceLock<GenericDetour<StartAnimFn>> = OnceLock::new();
pub static GET_PLAYER: OnceLock<GenericDetour<GetPtrFn>> = OnceLock::new();
pub static GET_PLAYER_ENTITY: OnceLock<GenericDetour<GetPtrFn>> = OnceLock::new();
pub static GET_PLAYER_POS: OnceLock<GenericDetour<GetPosFn>> = OnceLock::new();
pub static REQUEST_STATE: OnceLock<GenericDetour<RequestStateFn>> = OnceLock::new();

/// Original enemy update functions, by absolute vtable address: one per `sdk::UPDATE_SLOTS` entry.
static EM_UPDATE_ORIG: OnceLock<HashMap<usize, [usize; 2]>> = OnceLock::new();
/// The `EmBase` vtable's own entries, for an object whose vtable is not in the map (never expected).
static EM_UPDATE_FALLBACK: OnceLock<[usize; 2]> = OnceLock::new();
static EM_UPDATE_MISS_LOGGED: AtomicBool = AtomicBool::new(false);

unsafe extern "system" fn get_player_detour() -> *mut c_void {
    crate::sync::aggro::get_player()
}

unsafe extern "system" fn get_player_entity_detour() -> *mut c_void {
    crate::sync::aggro::get_player_entity()
}

unsafe extern "system" fn get_player_pos_detour(out: *mut cVec4) -> *mut cVec4 {
    crate::sync::aggro::get_player_pos(out)
}

/// Calls the original getter, bypassing our detour.
pub fn call_get_player() -> *mut c_void {
    // SAFETY: original game function with its real signature.
    GET_PLAYER.get().map_or(core::ptr::null_mut(), |d| unsafe { d.call() })
}

pub fn call_get_player_entity() -> *mut c_void {
    // SAFETY: as above.
    GET_PLAYER_ENTITY.get().map_or(core::ptr::null_mut(), |d| unsafe { d.call() })
}

pub fn call_get_player_pos(out: *mut cVec4) -> *mut cVec4 {
    // SAFETY: as above; `out` is the caller's vector.
    GET_PLAYER_POS.get().map_or(out, |d| unsafe { d.call(out) })
}

unsafe extern "system" fn em_update_a(this: *mut c_void, a2: usize, a3: usize, a4: usize) -> usize {
    em_update(0, this, a2, a3, a4)
}

unsafe extern "system" fn em_update_b(this: *mut c_void, a2: usize, a3: usize, a4: usize) -> usize {
    em_update(1, this, a2, a3, a4)
}

/// An enemy's update, reached through its (patched) vtable: runs the original with the aggro context set.
fn em_update(i: usize, this: *mut c_void, a2: usize, a3: usize, a4: usize) -> usize {
    // SAFETY: we are only reachable through a patched vtable slot, so `this` is a live object whose first
    // field is that vtable.
    let vt = unsafe { (this as *const usize).read() };
    let orig = match EM_UPDATE_ORIG.get().and_then(|m| m.get(&vt)) {
        Some(o) => o[i],
        None => {
            if !EM_UPDATE_MISS_LOGGED.swap(true, Relaxed) {
                tracing::warn!("enemy update reached with an unknown vtable {vt:#x}; using EmBase's");
            }
            match EM_UPDATE_FALLBACK.get() {
                Some(f) => f[i],
                None => return 0,
            }
        }
    };
    // SAFETY: `orig` is the game's own function that was in this slot.
    let f: UpdateFn = unsafe { std::mem::transmute(orig) };
    crate::sync::aggro::during_update(this, || unsafe { f(this, a2, a3, a4) })
}

/// Overwrites one pointer in read-only data (a vtable slot); returns the previous value.
fn patch_ptr(at: usize, new: usize) -> Result<usize, String> {
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: `at` is an 8-byte vtable slot inside the game module (checked by the caller).
    unsafe {
        VirtualProtect(at as *const c_void, 8, PAGE_READWRITE, &mut old).map_err(|e| format!("VirtualProtect {at:#x}: {e}"))?;
        let prev = (*(at as *const AtomicUsize)).swap(new, Relaxed);
        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(at as *const c_void, 8, old, &mut ignored);
        Ok(prev)
    }
}

/// Routes every enemy class's update slots through `em_update`.
fn wrap_enemy_updates() -> Result<(), String> {
    let g = game();
    let a = &g.addr;
    let ours = [em_update_a as UpdateFn as usize, em_update_b as UpdateFn as usize];
    let mut orig = HashMap::new();
    for &vt in &a.em_vtables {
        let mut o = [0; 2];
        for (k, &slot) in sdk::UPDATE_SLOTS.iter().enumerate() {
            let f = rtti::vtable_slot(&g.img, vt, slot).map_err(|e| format!("EmBase vtable {vt:#x}: {e}"))?;
            o[k] = a.abs(f);
        }
        orig.insert(a.abs(vt), o);
    }
    let fallback = *orig.get(&a.abs(a.em_base_vtable)).ok_or("EmBase vtable missing from the enemy vtables")?;
    let _ = EM_UPDATE_FALLBACK.set(fallback);
    EM_UPDATE_ORIG.set(orig).map_err(|_| "enemy updates wrapped twice")?;
    // Only patch after the originals are published: an update can run as soon as a slot changes.
    for &vt in &a.em_vtables {
        for (k, &slot) in sdk::UPDATE_SLOTS.iter().enumerate() {
            let at = a.abs(vt) + 8 * slot;
            if !g.in_module(at) {
                return Err(format!("vtable slot {at:#x} outside the module"));
            }
            patch_ptr(at, ours[k])?;
        }
    }
    tracing::info!("wrapped the update slots {:?} of {} enemy classes", sdk::UPDATE_SLOTS, a.em_vtables.len());
    Ok(())
}

unsafe extern "system" fn spawn_detour(spawner: *mut c_void, params: *const SpawnParams) -> *mut Entity {
    crate::sync::entities::on_spawn(spawner, params)
}

unsafe extern "system" fn start_anim_detour(this: *mut c_void, anim: u32, variant: u32, a3: u32, a4: u32) {
    crate::sync::player::on_start_animation(this, anim, variant, a3, a4)
}

unsafe extern "system" fn em_start_anim_detour(this: *mut c_void, anim: u32, variant: u32, a3: u32, a4: u32) {
    crate::sync::entities::on_em_start_animation(this, anim, variant, a3, a4)
}

unsafe extern "system" fn request_state_detour(this: *mut SceneAction, req: *const StateReq) -> usize {
    crate::sync::events::on_request_state(this, req)
}

/// Calls the original `Action::request_state`, bypassing our detour.
pub fn call_request_state(this: *mut SceneAction, req: *const StateReq) -> usize {
    // SAFETY: original game function with its real signature; the arguments are the engine's own.
    REQUEST_STATE.get().map_or(0, |d| unsafe { d.call(this, req) })
}

/// Calls the game's spawn function, bypassing our detour.
pub fn call_spawn(spawner: *mut c_void, params: *const SpawnParams) -> *mut Entity {
    match SPAWN.get() {
        // SAFETY: original game function with its real signature; called on the game thread.
        Some(d) => unsafe { d.call(spawner, params) },
        None => core::ptr::null_mut(),
    }
}

/// Calls the original `Pl0000::start_animation`, bypassing our detour.
pub fn call_start_animation(this: *mut c_void, anim: u32, variant: u32, a3: u32, a4: u32) {
    if let Some(d) = START_ANIM.get() {
        // SAFETY: as in `call_spawn`; `this` is a validated Pl0000.
        unsafe { d.call(this, anim, variant, a3, a4) }
    }
}

/// Calls the original `EmBase::start_animation`, bypassing our detour.
pub fn call_em_start_animation(this: *mut c_void, anim: u32, variant: u32, a3: u32, a4: u32) {
    if let Some(d) = EM_START_ANIM.get() {
        // SAFETY: as in `call_spawn`; `this` is a validated EmBase.
        unsafe { d.call(this, anim, variant, a3, a4) }
    }
}

fn detour<T: retour::Function>(name: &str, slot: &OnceLock<GenericDetour<T>>, target: T, ours: T) -> Result<(), String>
where
    T: Copy,
{
    // SAFETY: `target` is a function start checked by `check_hook_site`, with signature `T`.
    let d = unsafe { GenericDetour::new(target, ours) }.map_err(|e| format!("{name} detour: {e}"))?;
    slot.set(d).map_err(|_| format!("{name} detour installed twice"))?;
    unsafe { slot.get().unwrap().enable() }.map_err(|e| format!("{name} detour enable: {e}"))?;
    tracing::info!("detoured {name}");
    Ok(())
}

// -------------------------------------------------------------------------------------------------------

fn on_update(_: &mut Registers) {
    crate::sync::tick();
}

fn on_held_store(r: &mut Registers) {
    crate::sync::player::on_held(r.rcx as usize);
}

fn on_pod_no_target(r: &mut Registers, original: usize) -> usize {
    // rax = target entity (0 here means "none"), rdi = the Pl0000 firing, rbp = pod_shot's frame.
    if r.rax != 0 {
        return original;
    }
    match crate::sync::player::on_pod_free_aim(r.rdi as usize, (r.rbp as usize).wrapping_sub(0x60)) {
        Some(resume) => {
            r.rax = 1; // as if a target were found (nier_coop, verified in-game)
            resume
        }
        None => original,
    }
}

/// Installs every hook. Called once from the worker thread, before the game's main loop starts.
pub fn install(addr: Addresses, img: Image<'static>) -> Result<(), String> {
    GAME_BASE.store(addr.base, Relaxed);
    GAME.set(Game { addr, img }).map_err(|_| "hooks installed twice".to_string())?;
    let a = &game().addr;
    // SAFETY: resolved function starts in the game module with the documented signatures.
    unsafe {
        detour("spawn", &SPAWN, std::mem::transmute::<usize, SpawnFn>(a.abs(a.spawn)), spawn_detour as SpawnFn)?;
        detour(
            "start_animation",
            &START_ANIM,
            std::mem::transmute::<usize, StartAnimFn>(a.abs(a.start_animation)),
            start_anim_detour as StartAnimFn,
        )?;
        detour(
            "em_start_animation",
            &EM_START_ANIM,
            std::mem::transmute::<usize, StartAnimFn>(a.abs(a.em_start_animation)),
            em_start_anim_detour as StartAnimFn,
        )?;
        let ptr = |rva| std::mem::transmute::<usize, GetPtrFn>(a.abs(rva));
        detour("get_player", &GET_PLAYER, ptr(a.get_player), get_player_detour as GetPtrFn)?;
        detour("get_player_entity", &GET_PLAYER_ENTITY, ptr(a.get_player_entity), get_player_entity_detour as GetPtrFn)?;
        detour(
            "get_player_pos",
            &GET_PLAYER_POS,
            std::mem::transmute::<usize, GetPosFn>(a.abs(a.get_player_pos)),
            get_player_pos_detour as GetPosFn,
        )?;
        detour(
            "request_state",
            &REQUEST_STATE,
            std::mem::transmute::<usize, RequestStateFn>(a.abs(a.request_state)),
            request_state_detour as RequestStateFn,
        )?;
    }
    wrap_enemy_updates()?;
    mid_hook("held_store", a.abs(a.held_store), on_held_store)?;
    ret_hook("pod_no_target", a.abs(a.pod_no_target), on_pod_no_target)?;
    // Last: the per-frame tick starts the sync logic, which uses the hooks above.
    mid_hook("update", a.abs(a.update), on_update)?;
    Ok(())
}
