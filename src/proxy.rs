//! `dinput8.dll` proxy. The game imports `DINPUT8.dll` statically, so Windows loads this DLL from the game
//! folder at startup; no injector needed. `DirectInput8Create` forwards to the real system DLL.
//!
//! It also blocks game input while the overlay is open: the game reads keyboard and mouse through
//! DirectInput, not window messages, so on each device it creates we patch `GetDeviceState` and
//! `GetDeviceData` to return "nothing pressed" while `state::OVERLAY_VISIBLE` is set.
//!
//! In gameplay the game holds the mouse in exclusive mode, which freezes the Windows cursor and stops mouse
//! messages from reaching the overlay (the pause menu doesn't, which is why the overlay worked there). So
//! while the overlay is open each device is also unacquired, and re-acquired on the first read after it closes.
//!
//! Two copies of the game can run side by side, and only the focused one may act on input. DirectInput
//! reads are held the same way while this window is not in front. XInput and `GetAsyncKeyState` read the
//! hardware whatever has focus, so `gate_unfocused_input` repoints the game exe's imports of both.

use std::ffi::c_void;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{E_FAIL, HMODULE, HWND, LPARAM, LRESULT, S_OK, WPARAM};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GWLP_WNDPROC, GetForegroundWindow, GetWindowThreadProcessId, IsWindow, SetWindowLongPtrW, WA_INACTIVE,
    WM_ACTIVATE, WM_ACTIVATEAPP, WM_KILLFOCUS, WNDPROC,
};
use windows::core::{HRESULT, PCSTR, PCWSTR, s, w};

type DirectInput8CreateFn = unsafe extern "system" fn(HMODULE, u32, *const c_void, *mut *mut c_void, *mut c_void) -> HRESULT;
type CreateDeviceFn = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void, *mut c_void) -> HRESULT;
type GetDeviceStateFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> HRESULT;
type GetDeviceDataFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32, u32) -> HRESULT;

/// COM vtable slots: IDirectInput8::CreateDevice, IDirectInputDevice8::GetDeviceState / GetDeviceData.
const SLOT_CREATE_DEVICE: usize = 3;
const SLOT_ACQUIRE: usize = 7;
const SLOT_UNACQUIRE: usize = 8;
const SLOT_GET_DEVICE_STATE: usize = 9;
const SLOT_GET_DEVICE_DATA: usize = 10;

static REAL: OnceLock<Option<DirectInput8CreateFn>> = OnceLock::new();

fn real() -> Option<DirectInput8CreateFn> {
    *REAL.get_or_init(|| unsafe {
        let mut buf = [0u16; 260];
        let n = GetSystemDirectoryW(Some(&mut buf)) as usize;
        if n == 0 || n >= buf.len() - 20 {
            return None;
        }
        let path: Vec<u16> = buf[..n].iter().copied().chain("\\dinput8.dll\0".encode_utf16()).collect();
        let h = LoadLibraryW(PCWSTR(path.as_ptr())).ok()?;
        let f = GetProcAddress(h, s!("DirectInput8Create"))?;
        Some(std::mem::transmute::<unsafe extern "system" fn() -> isize, DirectInput8CreateFn>(f))
    })
}

/// Original function pointers per patched vtable (keyed by vtable address).
static CREATE_DEVICE: Mutex<Vec<(usize, CreateDeviceFn)>> = Mutex::new(Vec::new());
static DEVICE: Mutex<Vec<(usize, GetDeviceStateFn, GetDeviceDataFn)>> = Mutex::new(Vec::new());

/// Replaces vtable slot `slot` of `obj` with `f`; returns the previous function.
unsafe fn patch_slot(obj: *mut c_void, slot: usize, f: usize) -> Option<(usize, usize)> {
    unsafe {
        let vtable = *(obj as *const *mut usize);
        if vtable.is_null() {
            return None;
        }
        let entry = vtable.add(slot);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        VirtualProtect(entry.cast(), size_of::<usize>(), PAGE_READWRITE, &mut old).ok()?;
        let prev = entry.read();
        entry.write(f);
        let _ = VirtualProtect(entry.cast(), size_of::<usize>(), old, &mut old);
        Some((vtable as usize, prev))
    }
}

fn vtable_of(obj: *mut c_void) -> usize {
    unsafe { obj.cast::<usize>().as_ref().copied().unwrap_or(0) }
}

/// Devices (by object address) unacquired here while the overlay was open.
static RELEASED: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Calls `Acquire` or `Unacquire` (both `HRESULT (this)`) through the device's own vtable.
unsafe fn call_slot(dev: *mut c_void, slot: usize) {
    unsafe {
        let Some(vtable) = dev.cast::<*const usize>().as_ref().copied().filter(|v| !v.is_null()) else { return };
        let f = vtable.add(slot).read();
        if f != 0 {
            let _ = std::mem::transmute::<usize, unsafe extern "system" fn(*mut c_void) -> HRESULT>(f)(dev);
        }
    }
}

/// True when the foreground window belongs to this game process.
pub fn focused() -> bool {
    // SAFETY: plain Win32 queries.
    unsafe {
        let mut pid = 0;
        GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
        pid == GetCurrentProcessId()
    }
}

/// Called before each read. While the overlay is open or the window is not focused, unacquires the device
/// (an exclusive-mode mouse otherwise freezes the cursor and swallows mouse messages) and returns true.
/// Afterwards, re-acquires the devices released here.
fn overlay_holds(dev: *mut c_void) -> bool {
    let visible = crate::state::OVERLAY_VISIBLE.load(Relaxed) || !focused();
    let Ok(mut rel) = RELEASED.lock() else { return visible };
    let i = rel.iter().position(|&d| d == dev as usize);
    if visible {
        // Every call, in case the game re-acquired it meanwhile (a no-op when already unacquired).
        unsafe { call_slot(dev, SLOT_UNACQUIRE) };
        if i.is_none() {
            rel.push(dev as usize);
        }
    } else if let Some(i) = i {
        rel.swap_remove(i);
        unsafe { call_slot(dev, SLOT_ACQUIRE) };
    }
    visible
}

unsafe extern "system" fn get_device_state(this: *mut c_void, size: u32, data: *mut c_void) -> HRESULT {
    let vt = vtable_of(this);
    let Some(orig) = DEVICE.lock().ok().and_then(|d| d.iter().find(|e| e.0 == vt).map(|e| e.1)) else { return E_FAIL };
    if overlay_holds(this) {
        if !data.is_null() {
            unsafe { std::ptr::write_bytes(data.cast::<u8>(), 0, size as usize) };
        }
        return S_OK;
    }
    unsafe { orig(this, size, data) }
}

unsafe extern "system" fn get_device_data(this: *mut c_void, size: u32, data: *mut c_void, count: *mut u32, flags: u32) -> HRESULT {
    let vt = vtable_of(this);
    let Some(orig) = DEVICE.lock().ok().and_then(|d| d.iter().find(|e| e.0 == vt).map(|e| e.2)) else { return E_FAIL };
    if overlay_holds(this) {
        if !count.is_null() {
            unsafe { *count = 0 };
        }
        return S_OK;
    }
    unsafe { orig(this, size, data, count, flags) }
}

unsafe extern "system" fn create_device(this: *mut c_void, guid: *const c_void, out: *mut *mut c_void, outer: *mut c_void) -> HRESULT {
    let vt = vtable_of(this);
    let Some(orig) = CREATE_DEVICE.lock().ok().and_then(|d| d.iter().find(|e| e.0 == vt).map(|e| e.1)) else { return E_FAIL };
    let hr = unsafe { orig(this, guid, out, outer) };
    if hr.is_ok()
        && let Some(dev) = unsafe { out.as_ref() }.copied().filter(|d| !d.is_null())
        && let Ok(mut list) = DEVICE.lock()
    {
        let dvt = vtable_of(dev);
        if !list.iter().any(|e| e.0 == dvt) {
            unsafe {
                if let (Some((_, s)), Some((_, d))) = (
                    patch_slot(dev, SLOT_GET_DEVICE_STATE, get_device_state as *const () as usize),
                    patch_slot(dev, SLOT_GET_DEVICE_DATA, get_device_data as *const () as usize),
                ) {
                    list.push((dvt, std::mem::transmute::<usize, GetDeviceStateFn>(s), std::mem::transmute::<usize, GetDeviceDataFn>(d)));
                }
            }
        }
    }
    hr
}

static REAL_XINPUT_GET_STATE: OnceLock<usize> = OnceLock::new();
static REAL_GET_ASYNC_KEY_STATE: OnceLock<usize> = OnceLock::new();

/// `XInputGetState`: the pad reads idle (packet number kept) while this window is not focused.
unsafe extern "system" fn xinput_get_state(user: u32, state: *mut u8) -> u32 {
    let Some(&f) = REAL_XINPUT_GET_STATE.get() else { return 1167 }; // ERROR_DEVICE_NOT_CONNECTED
    let r = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(u32, *mut u8) -> u32>(f)(user, state) };
    if r == 0 && !state.is_null() && !focused() {
        // XINPUT_STATE: dwPacketNumber (0x00), XINPUT_GAMEPAD (0x04..0x10).
        unsafe { std::ptr::write_bytes(state.add(4), 0, 12) };
    }
    r
}

/// `GetAsyncKeyState`: nothing is pressed while this window is not focused.
unsafe extern "system" fn get_async_key_state(vk: i32) -> i16 {
    let Some(&f) = REAL_GET_ASYNC_KEY_STATE.get() else { return 0 };
    if !focused() {
        return 0;
    }
    unsafe { std::mem::transmute::<usize, unsafe extern "system" fn(i32) -> i16>(f)(vk) }
}

static REAL_GET_FOREGROUND_WINDOW: OnceLock<usize> = OnceLock::new();
/// The last foreground window that belonged to this process (the game's own window).
static GAME_WINDOW: AtomicUsize = AtomicUsize::new(0);

/// `GetForegroundWindow` as the game sees it: always its own window once it has been in front, so the game
/// never notices losing focus and does not open its pause menu (two copies run side by side). Input is still
/// gated by real focus: `focused()` calls the real function through this DLL's own import, not the game's.
unsafe extern "system" fn get_foreground_window() -> usize {
    let Some(&f) = REAL_GET_FOREGROUND_WINDOW.get() else { return 0 };
    let real = unsafe { std::mem::transmute::<usize, unsafe extern "system" fn() -> usize>(f)() };
    let mut pid = 0;
    // SAFETY: plain Win32 queries; a stale handle just fails them.
    unsafe {
        GetWindowThreadProcessId(HWND(real as *mut c_void), Some(&mut pid));
        if real != 0 && pid == GetCurrentProcessId() {
            GAME_WINDOW.store(real, Relaxed);
            return real;
        }
        let ours = GAME_WINDOW.load(Relaxed);
        if ours != 0 && IsWindow(Some(HWND(ours as *mut c_void))).as_bool() { ours } else { real }
    }
}

/// The game's own window procedure, wrapped by `focus_wnd_proc`.
static GAME_WND_PROC: AtomicUsize = AtomicUsize::new(0);

/// Swallows the messages that tell the game it lost focus, which open its pause menu. Activation messages
/// still pass, so the window regains keyboard focus normally.
unsafe extern "system" fn focus_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    let losing = match msg {
        WM_ACTIVATEAPP => wparam.0 == 0,
        WM_ACTIVATE => (wparam.0 & 0xFFFF) as u32 == WA_INACTIVE,
        WM_KILLFOCUS => true,
        _ => false,
    };
    if losing {
        crate::ui::cursor::release(); // the game would not lift its cursor clip: it never sees the focus loss
        return LRESULT(0);
    }
    // SAFETY: GAME_WND_PROC holds the procedure SetWindowLongPtrW returned for this window.
    unsafe { CallWindowProcW(std::mem::transmute::<usize, WNDPROC>(GAME_WND_PROC.load(Relaxed)), hwnd, msg, wparam, lparam) }
}

/// Wraps the game window's procedure with `focus_wnd_proc`, so an unfocused copy keeps running. Called every
/// frame on the render thread; does nothing until the game window is known, or once it is wrapped. The order
/// against hudhook's own subclass does not matter: this one only swallows focus-loss messages.
pub fn keep_running_unfocused() {
    let hwnd = GAME_WINDOW.load(Relaxed);
    if hwnd == 0 || GAME_WND_PROC.load(Relaxed) != 0 {
        return;
    }
    // SAFETY: `hwnd` is this process's game window (seen as the foreground window of this process).
    let prev = unsafe { SetWindowLongPtrW(HWND(hwnd as *mut c_void), GWLP_WNDPROC, focus_wnd_proc as *const () as isize) };
    if prev == 0 {
        GAME_WND_PROC.store(1, Relaxed); // not installed; marks "tried" so this logs once
        tracing::error!("could not wrap the game window procedure; an unfocused copy will still pause");
        return;
    }
    GAME_WND_PROC.store(prev as usize, Relaxed);
    tracing::info!("focus-loss messages are hidden from the game: an unfocused copy keeps running");
}

/// Repoints the game exe's import-address-table entries for `XInputGetState` (XINPUT1_4 ordinal 2) and
/// `GetAsyncKeyState` to the focus-gated wrappers above, and `GetForegroundWindow` so the game keeps running
/// unfocused. `module` is the live exe image at `base`.
pub fn gate_unfocused_input(module: &[u8], base: usize) {
    let rd = |o: usize| module.get(o..o + 4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
    // PE: e_lfanew at 0x3C; "PE\0\0"; PE32+ optional header at +0x18, its IAT data directory (entry 12) at +0xD0.
    let Some((iat, size)) = rd(0x3C)
        .filter(|&nt| rd(nt) == Some(0x4550) && module.get(nt + 0x18..nt + 0x1A) == Some(&[0x0B, 0x02]))
        .and_then(|nt| Some((rd(nt + 0x18 + 0xD0)?, rd(nt + 0x18 + 0xD4)?)))
        .filter(|&(iat, size)| size > 0 && iat.checked_add(size).is_some_and(|e| e <= module.len()))
    else {
        tracing::error!("game exe has no readable import address table; both game copies will read all input");
        return;
    };
    // SAFETY: plain module loads; ordinal 2 is passed as a MAKEINTRESOURCE-style pointer.
    let targets = unsafe {
        [
            ("XInputGetState", LoadLibraryW(w!("xinput1_4.dll")).ok().and_then(|h| GetProcAddress(h, PCSTR(2 as *const u8))),
                &REAL_XINPUT_GET_STATE, xinput_get_state as *const () as usize),
            ("GetAsyncKeyState", LoadLibraryW(w!("user32.dll")).ok().and_then(|h| GetProcAddress(h, s!("GetAsyncKeyState"))),
                &REAL_GET_ASYNC_KEY_STATE, get_async_key_state as *const () as usize),
            ("GetForegroundWindow", LoadLibraryW(w!("user32.dll")).ok().and_then(|h| GetProcAddress(h, s!("GetForegroundWindow"))),
                &REAL_GET_FOREGROUND_WINDOW, get_foreground_window as *const () as usize),
        ]
    };
    for (name, real, slot, hook) in targets {
        let Some(real) = real.map(|f| f as usize) else {
            tracing::error!("{name} not found; unfocused game copies still read it");
            continue;
        };
        let _ = slot.set(real);
        let mut patched = 0;
        for off in (iat..iat + size - size % 8).step_by(8) {
            // SAFETY: `off` lies inside the IAT of the live module (bounds checked above).
            unsafe {
                let entry = (base + off) as *mut usize;
                if entry.read_volatile() != real {
                    continue;
                }
                let mut old = PAGE_PROTECTION_FLAGS(0);
                if VirtualProtect(entry.cast(), 8, PAGE_READWRITE, &mut old).is_ok() {
                    entry.write_volatile(hook);
                    let _ = VirtualProtect(entry.cast(), 8, old, &mut old);
                    patched += 1;
                }
            }
        }
        if patched == 0 {
            tracing::warn!("{name}: no import entry found; unfocused game copies still read it");
        } else {
            tracing::info!("{name}: patched for side-by-side copies ({patched} import entries)");
        }
    }
}

/// # Safety
/// Called by the game with the documented `DirectInput8Create` arguments.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn DirectInput8Create(
    hinst: HMODULE,
    version: u32,
    riid: *const c_void,
    out: *mut *mut c_void,
    outer: *mut c_void,
) -> HRESULT {
    let Some(f) = real() else { return E_FAIL };
    let hr = unsafe { f(hinst, version, riid, out, outer) };
    if hr.is_ok()
        && let Some(di) = unsafe { out.as_ref() }.copied().filter(|d| !d.is_null())
        && let Ok(mut list) = CREATE_DEVICE.lock()
    {
        let vt = vtable_of(di);
        if !list.iter().any(|e| e.0 == vt)
            && let Some((_, prev)) = unsafe { patch_slot(di, SLOT_CREATE_DEVICE, create_device as *const () as usize) }
        {
            list.push((vt, unsafe { std::mem::transmute::<usize, CreateDeviceFn>(prev) }));
        }
    }
    hr
}
