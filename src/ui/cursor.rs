//! Frees the Windows cursor while the overlay is open. In gameplay the game calls `SetCursorPos` every
//! frame to pin the cursor to the window centre (and may `ClipCursor` it), which makes the overlay's mouse
//! snap back. These user32 detours drop those calls while the overlay is open, and `GetCursorPos` reports
//! the position the game last set, so the game sees no mouse movement and doesn't turn the camera.

use std::sync::OnceLock;
use std::sync::atomic::AtomicI64;
use std::sync::atomic::Ordering::Relaxed;

use retour::GenericDetour;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::w;

type SetCursorPosFn = unsafe extern "system" fn(i32, i32) -> i32;
type ClipCursorFn = unsafe extern "system" fn(*const RECT) -> i32;
type GetCursorPosFn = unsafe extern "system" fn(*mut POINT) -> i32;

static SET_POS: OnceLock<GenericDetour<SetCursorPosFn>> = OnceLock::new();
static CLIP: OnceLock<GenericDetour<ClipCursorFn>> = OnceLock::new();
static GET_POS: OnceLock<GenericDetour<GetCursorPosFn>> = OnceLock::new();

/// The last position the game passed to `SetCursorPos` (x in the high 32 bits, y in the low), or
/// `i64::MIN` if it never called it.
static GAME_POS: AtomicI64 = AtomicI64::new(i64::MIN);

/// The game must leave the cursor alone: the overlay is open, or another window (say, the second copy of the
/// game) is in front. The game never learns it lost focus (see `proxy::focus_wnd_proc`), so it would keep
/// pinning and clipping the cursor to its own window.
fn open() -> bool {
    crate::state::OVERLAY_VISIBLE.load(Relaxed) || !crate::proxy::focused()
}

unsafe extern "system" fn set_cursor_pos(x: i32, y: i32) -> i32 {
    GAME_POS.store(((x as i64) << 32) | (y as u32 as i64), Relaxed);
    if open() {
        return 1;
    }
    SET_POS.get().map_or(0, |d| unsafe { d.call(x, y) })
}

unsafe extern "system" fn clip_cursor(rect: *const RECT) -> i32 {
    if open() {
        return 1;
    }
    CLIP.get().map_or(0, |d| unsafe { d.call(rect) })
}

unsafe extern "system" fn get_cursor_pos(out: *mut POINT) -> i32 {
    let p = GAME_POS.load(Relaxed);
    if open()
        && p != i64::MIN
        && let Some(out) = unsafe { out.as_mut() }
    {
        *out = POINT { x: (p >> 32) as i32, y: p as i32 };
        return 1;
    }
    GET_POS.get().map_or(0, |d| unsafe { d.call(out) })
}

/// Called when the overlay opens or the window loses focus: lift any clip the game set before.
pub fn release() {
    if let Some(d) = CLIP.get() {
        // SAFETY: the original ClipCursor; null means "no clip".
        unsafe { d.call(std::ptr::null()) };
    }
}

fn hook<T: retour::Function + Copy>(name: &str, slot: &OnceLock<GenericDetour<T>>, ours: T) -> Result<(), String> {
    // SAFETY: user32 is loaded in every GUI process; the export is looked up by name and has signature `T`.
    let target = unsafe {
        let user32 = GetModuleHandleW(w!("user32.dll")).map_err(|e| format!("user32: {e}"))?;
        let name_c = std::ffi::CString::new(name).map_err(|e| e.to_string())?;
        let f = GetProcAddress(user32, windows::core::PCSTR(name_c.as_ptr().cast())).ok_or(format!("{name} not exported"))?;
        std::mem::transmute_copy::<_, T>(&f)
    };
    let d = unsafe { GenericDetour::new(target, ours) }.map_err(|e| format!("{name} detour: {e}"))?;
    let d = slot.get_or_init(|| d);
    unsafe { d.enable() }.map_err(|e| format!("{name} enable: {e}"))
}

pub fn install() {
    for r in [
        hook("SetCursorPos", &SET_POS, set_cursor_pos as SetCursorPosFn),
        hook("ClipCursor", &CLIP, clip_cursor as ClipCursorFn),
        hook("GetCursorPos", &GET_POS, get_cursor_pos as GetCursorPosFn),
    ] {
        if let Err(e) = r {
            tracing::warn!("cursor hook: {e} (the overlay's mouse may stick to the centre in gameplay)");
        }
    }
}
