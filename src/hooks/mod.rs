//! Hook installation. Hooks only copy data in or out and return; networking never runs inside a hook
//! (CLAUDE.md architecture rule 2). The sync logic they call lives in `crate::sync`.
//!
//! - Mid-function and entry hooks use `ilhook`, through a shim that also preserves xmm4/xmm5 (ilhook saves
//!   only xmm0-3, but xmm4/5 can be live in the middle of a function) and gives the callback a full
//!   0x20-byte shadow area.
//! - Function detours with a known signature use `retour::GenericDetour`.
//!
//! Every site was checked by `scan::Image::check_hook_site` in `addr::resolve` before we get here.

use std::ffi::c_void;
use std::sync::OnceLock;

use ilhook::x64::{CallbackOption, HookFlags, HookType, Hooker, Registers};
use retour::GenericDetour;

use crate::addr::Addresses;
use crate::scan::Image;
use crate::sdk::{Entity, SpawnParams};

/// Resolved addresses plus the live module image (for RTTI checks and module-range checks).
pub struct Game {
    pub addr: Addresses,
    pub img: Image<'static>,
}

static GAME: OnceLock<Game> = OnceLock::new();

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

/// `bool Action::request_state(this, const StateReq*)`; the bool comes back in al (upper bits undefined).
pub type RequestStateFn = unsafe extern "system" fn(*mut crate::sdk::SceneAction, *const crate::sdk::StateReq) -> usize;

pub static SPAWN: OnceLock<GenericDetour<SpawnFn>> = OnceLock::new();
pub static START_ANIM: OnceLock<GenericDetour<StartAnimFn>> = OnceLock::new();
pub static EM_START_ANIM: OnceLock<GenericDetour<StartAnimFn>> = OnceLock::new();
pub static REQUEST_STATE: OnceLock<GenericDetour<RequestStateFn>> = OnceLock::new();
/// `bool post(queue, const Sender*, const StateReq* signal)`: every scene signal goes through here.
pub type SignalPostFn =
    unsafe extern "system" fn(usize, *const crate::sdk::SignalSender, *const crate::sdk::StateReq) -> usize;
pub static SIGNAL_POST: OnceLock<GenericDetour<SignalPostFn>> = OnceLock::new();

unsafe extern "system" fn signal_post_detour(
    queue: usize,
    from: *const crate::sdk::SignalSender,
    sig: *const crate::sdk::StateReq,
) -> usize {
    crate::sync::events::on_post(queue, from, sig)
}

/// Calls the original signal post, bypassing our detour.
pub fn call_signal_post(queue: usize, from: *const crate::sdk::SignalSender, sig: *const crate::sdk::StateReq) -> usize {
    match SIGNAL_POST.get() {
        // SAFETY: as in `call_spawn`; arguments come from the game's own call or are built like it builds them.
        Some(d) => unsafe { d.call(queue, from, sig) },
        None => 0,
    }
}

unsafe extern "system" fn request_state_detour(this: *mut crate::sdk::SceneAction, req: *const crate::sdk::StateReq) -> usize {
    crate::sync::scene::on_request(this, req)
}

/// Calls the original `Action::request_state`, bypassing our detour.
pub fn call_request_state(this: *mut crate::sdk::SceneAction, req: *const crate::sdk::StateReq) -> usize {
    match REQUEST_STATE.get() {
        // SAFETY: as in `call_spawn`; arguments come straight from the game's own call.
        Some(d) => unsafe { d.call(this, req) },
        None => 0,
    }
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
        detour(
            "request_state",
            &REQUEST_STATE,
            std::mem::transmute::<usize, RequestStateFn>(a.abs(a.request_state)),
            request_state_detour as RequestStateFn,
        )?;
        detour(
            "signal_post",
            &SIGNAL_POST,
            std::mem::transmute::<usize, SignalPostFn>(a.abs(a.signal_post)),
            signal_post_detour as SignalPostFn,
        )?;
    }
    mid_hook("held_store", a.abs(a.held_store), on_held_store)?;
    ret_hook("pod_no_target", a.abs(a.pod_no_target), on_pod_no_target)?;
    // Last: the per-frame tick starts the sync logic, which uses the hooks above.
    mid_hook("update", a.abs(a.update), on_update)?;
    Ok(())
}
