//! `dinput8.dll` proxy. The game imports `DINPUT8.dll` statically, so Windows loads this DLL from the game
//! folder at startup; no injector needed. `DirectInput8Create` forwards to the real system DLL.
//!
//! It also blocks game input while the overlay is open: the game reads keyboard and mouse through
//! DirectInput, not window messages, so on each device it creates we patch `GetDeviceState` and
//! `GetDeviceData` to return "nothing pressed" while `state::OVERLAY_VISIBLE` is set.

use std::ffi::c_void;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{E_FAIL, HMODULE};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{PAGE_PROTECTION_FLAGS, PAGE_READWRITE, VirtualProtect};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::core::{HRESULT, PCWSTR, s};

type DirectInput8CreateFn = unsafe extern "system" fn(HMODULE, u32, *const c_void, *mut *mut c_void, *mut c_void) -> HRESULT;
type CreateDeviceFn = unsafe extern "system" fn(*mut c_void, *const c_void, *mut *mut c_void, *mut c_void) -> HRESULT;
type GetDeviceStateFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void) -> HRESULT;
type GetDeviceDataFn = unsafe extern "system" fn(*mut c_void, u32, *mut c_void, *mut u32, u32) -> HRESULT;

/// COM vtable slots: IDirectInput8::CreateDevice, IDirectInputDevice8::GetDeviceState / GetDeviceData.
const SLOT_CREATE_DEVICE: usize = 3;
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

unsafe extern "system" fn get_device_state(this: *mut c_void, size: u32, data: *mut c_void) -> HRESULT {
    let vt = vtable_of(this);
    let Some(orig) = DEVICE.lock().ok().and_then(|d| d.iter().find(|e| e.0 == vt).map(|e| e.1)) else { return E_FAIL };
    let hr = unsafe { orig(this, size, data) };
    if hr.is_ok() && crate::state::OVERLAY_VISIBLE.load(Relaxed) && !data.is_null() {
        unsafe { std::ptr::write_bytes(data.cast::<u8>(), 0, size as usize) };
    }
    hr
}

unsafe extern "system" fn get_device_data(this: *mut c_void, size: u32, data: *mut c_void, count: *mut u32, flags: u32) -> HRESULT {
    let vt = vtable_of(this);
    let Some(orig) = DEVICE.lock().ok().and_then(|d| d.iter().find(|e| e.0 == vt).map(|e| e.2)) else { return E_FAIL };
    let hr = unsafe { orig(this, size, data, count, flags) };
    if hr.is_ok() && crate::state::OVERLAY_VISIBLE.load(Relaxed) && !count.is_null() {
        unsafe { *count = 0 }; // the buffered events were consumed, so they don't fire after closing
    }
    hr
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
