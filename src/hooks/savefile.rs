//! Save redirection, client side. The game opens every save file by name through `CreateFileA` on its save
//! worker (`notes/2026-11-22-save-files-reversing.md`), so detouring `CreateFileA` / `DeleteFileA` decides
//! which world the client can load:
//!
//! - **Host-world mode** (on from the first client session until the game restarts, unless nothing was loaded
//!   from the host): a read of `SlotData_N.dat` is served from the host's save (`host_save_path`) when N is the
//!   host's slot, and fails with "file not found" for every other slot, so the client cannot load one of its own
//!   worlds. Every slot write goes to `discard_path` and every slot delete does nothing: the client's own saves
//!   are never touched, and the host's save stays the host's (CLAUDE.md Phase 8: the client never sets progress
//!   from its own gameplay).
//! - **Hold** (a "Join" at startup, nier_coop `hooks/savefile.rs`): the game lists the saves once,
//!   after the startup logos, so the game's first slot read waits until the host's save has arrived (or `HOLD`
//!   passes). The save list then shows the host's world.
//!
//! Our own file I/O uses `CreateFileW` (Rust's std), which is not hooked.

use std::ffi::{CStr, c_void};
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};
use std::time::{Duration, Instant};

use retour::GenericDetour;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, SetLastError};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::core::{s, w};

/// Every `SlotData_N.dat` is exactly this size (Steam build 7788); anything else is never written.
pub const SAVE_SIZE: usize = 235_980;
/// Save slots the game offers (`SlotData_0..2`).
pub const SLOTS: u32 = 3;
/// Longest the game's first save read waits for the host's save.
const HOLD: Duration = Duration::from_secs(600);
/// Ends a hold early: the join was cancelled or failed, or the host has no save to send.
static HOLD_RELEASED: AtomicBool = AtomicBool::new(false);
/// Slot reads within this time of the first one are the startup save list, later ones are loads.
const LIST_WINDOW: Duration = Duration::from_secs(10);
const GENERIC_WRITE: u32 = 0x4000_0000;
const OPEN_EXISTING: u32 = 3;
const INVALID_HANDLE: *mut c_void = usize::MAX as *mut c_void;

/// Redirect slot files to the host's world.
static HOST_WORLD: AtomicBool = AtomicBool::new(false);
/// Hold the game's next slot read until `SAVE_READY`.
static HOLD_NEXT: AtomicBool = AtomicBool::new(false);
/// The host's save is at `host_save_path`, for slot `HOST_SLOT`.
static SAVE_READY: AtomicBool = AtomicBool::new(false);
static HOST_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
/// The slot of our own that the game loaded last (u32::MAX: none seen). The host sends that one.
static LOADED_SLOT: AtomicU32 = AtomicU32::new(u32::MAX);
static FIRST_READ: OnceLock<Instant> = OnceLock::new();
/// Where the world that was loaded last came from.
static ORIGIN: AtomicU8 = AtomicU8::new(Origin::None as u8);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Origin {
    None = 0,
    /// One of this player's own saves.
    Local = 1,
    /// The host's save.
    Host = 2,
}

type CreateA = unsafe extern "system" fn(*const u8, u32, u32, *const c_void, u32, u32, *mut c_void) -> *mut c_void;
type CreateW = unsafe extern "system" fn(*const u16, u32, u32, *const c_void, u32, u32, *mut c_void) -> *mut c_void;
type DeleteA = unsafe extern "system" fn(*const u8) -> i32;

static CREATE_A: OnceLock<GenericDetour<CreateA>> = OnceLock::new();
static DELETE_A: OnceLock<GenericDetour<DeleteA>> = OnceLock::new();
static CREATE_W: OnceLock<CreateW> = OnceLock::new();

/// Folder (next to the game exe, outside the Steam-Cloud-synced save folder) for the files below.
pub fn data_dir() -> PathBuf {
    crate::config::game_dir().join("nier_online")
}

/// The host's save as last received.
pub fn host_save_path() -> PathBuf {
    data_dir().join("host_save.dat")
}

/// Where the client's own saves go while it plays the host's world.
fn discard_path() -> PathBuf {
    data_dir().join("client_saves_discarded.dat")
}

fn ensure_dir() {
    let _ = std::fs::create_dir_all(data_dir());
}

pub fn install() -> Result<(), String> {
    ensure_dir();
    // KernelBase holds the real functions; kernel32's exports jump there.
    // SAFETY: looking up exported functions of a loaded system DLL, then detouring them with their documented
    // signatures.
    unsafe {
        let m = GetModuleHandleW(w!("kernelbase.dll")).map_err(|e| format!("kernelbase: {e}"))?;
        let a = GetProcAddress(m, s!("CreateFileA")).ok_or("CreateFileA not found")?;
        let wf = GetProcAddress(m, s!("CreateFileW")).ok_or("CreateFileW not found")?;
        let d = GetProcAddress(m, s!("DeleteFileA")).ok_or("DeleteFileA not found")?;
        let _ = CREATE_W.set(std::mem::transmute::<_, CreateW>(wf));
        let ca = GenericDetour::new(std::mem::transmute::<_, CreateA>(a), create_a as CreateA).map_err(|e| format!("CreateFileA: {e}"))?;
        let da = GenericDetour::new(std::mem::transmute::<_, DeleteA>(d), delete_a as DeleteA).map_err(|e| format!("DeleteFileA: {e}"))?;
        CREATE_A.set(ca).map_err(|_| "save hooks installed twice")?;
        DELETE_A.set(da).map_err(|_| "save hooks installed twice")?;
        CREATE_A.get().unwrap().enable().map_err(|e| format!("CreateFileA: {e}"))?;
        DELETE_A.get().unwrap().enable().map_err(|e| format!("DeleteFileA: {e}"))?;
    }
    // A host save from an earlier session stays usable until a new one arrives.
    if let Some(slot) = std::fs::read(data_dir().join("host_slot.txt")).ok().and_then(|b| String::from_utf8(b).ok()).and_then(|s| s.trim().parse::<u32>().ok())
        && slot < SLOTS
        && std::fs::metadata(host_save_path()).is_ok_and(|m| m.len() == SAVE_SIZE as u64)
    {
        HOST_SLOT.store(slot, Relaxed);
    }
    tracing::info!("save redirection installed");
    Ok(())
}

/// The slot number if `path` ends in `SlotData_<digit>.dat` (any case).
pub fn slot_of(path: &[u8]) -> Option<u32> {
    let file = path.rsplit(|&b| b == b'\\' || b == b'/').next()?.to_ascii_lowercase();
    (file.len() == 14 && file.starts_with(b"slotdata_") && file.ends_with(b".dat") && file[9].is_ascii_digit())
        .then(|| (file[9] - b'0') as u32)
}

fn wide(p: &std::path::Path) -> Vec<u16> {
    p.as_os_str().encode_wide().chain([0]).collect()
}

/// Opens `path` instead, with the caller's other arguments.
unsafe fn open_instead(path: PathBuf, access: u32, share: u32, sa: *const c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void {
    ensure_dir();
    let Some(f) = CREATE_W.get() else { return INVALID_HANDLE };
    let p = wide(&path);
    // SAFETY: the real CreateFileW with a NUL-terminated wide path and the game's own other arguments.
    unsafe { f(p.as_ptr(), access, share, sa, disp, flags, tmpl) }
}

fn not_found() -> *mut c_void {
    // SAFETY: plain Win32 call.
    unsafe { SetLastError(ERROR_FILE_NOT_FOUND) };
    INVALID_HANDLE
}

unsafe extern "system" fn create_a(name: *const u8, access: u32, share: u32, sa: *const c_void, disp: u32, flags: u32, tmpl: *mut c_void) -> *mut c_void {
    let orig = || match CREATE_A.get() {
        // SAFETY: the original function with the caller's arguments.
        Some(d) => unsafe { d.call(name, access, share, sa, disp, flags, tmpl) },
        None => INVALID_HANDLE,
    };
    // SAFETY: a NUL-terminated ANSI path from the caller, null-checked.
    let Some(slot) = (!name.is_null()).then(|| unsafe { CStr::from_ptr(name.cast()) }.to_bytes()).and_then(slot_of) else {
        return orig();
    };
    let write = access & GENERIC_WRITE != 0 || disp != OPEN_EXISTING;
    let mut load = false;
    if !write {
        if HOLD_NEXT.swap(false, Relaxed) {
            hold();
        }
        // After the hold, so the rest of the startup list still counts as the list.
        let first = *FIRST_READ.get_or_init(Instant::now);
        load = first.elapsed() > LIST_WINDOW;
    }
    if !HOST_WORLD.load(Relaxed) {
        if load {
            ORIGIN.store(Origin::Local as u8, Relaxed);
            LOADED_SLOT.store(slot, Relaxed);
        }
        return orig();
    }
    if write {
        tracing::info!("save redirection: the game saves to slot {slot}; kept out of your own saves (host's world)");
        // SAFETY: same arguments, another path.
        return unsafe { open_instead(discard_path(), access, share, sa, disp, flags, tmpl) };
    }
    if HOST_SLOT.load(Relaxed) == slot {
        if load {
            tracing::info!("save redirection: loading the host's world (slot {slot})");
            ORIGIN.store(Origin::Host as u8, Relaxed);
        }
        // SAFETY: as above.
        return unsafe { open_instead(host_save_path(), access, share, sa, disp, flags, tmpl) };
    }
    if load {
        tracing::info!("save redirection: slot {slot} is one of your own worlds; blocked while playing the host's");
    }
    not_found()
}

unsafe extern "system" fn delete_a(name: *const u8) -> i32 {
    // SAFETY: as in `create_a`.
    let slot = (!name.is_null()).then(|| unsafe { CStr::from_ptr(name.cast()) }.to_bytes()).and_then(slot_of);
    if slot.is_some() && HOST_WORLD.load(Relaxed) {
        tracing::info!("save redirection: kept your slot {} from being deleted (host's world)", slot.unwrap_or(0));
        return 1;
    }
    match DELETE_A.get() {
        // SAFETY: the original function with the caller's argument.
        Some(d) => unsafe { d.call(name) },
        None => 0,
    }
}

fn hold() {
    let start = Instant::now();
    tracing::info!("save sync: the game is reading saves; waiting for the host's save...");
    while !SAVE_READY.load(Relaxed) && !HOLD_RELEASED.load(Relaxed) && start.elapsed() < HOLD {
        std::thread::sleep(Duration::from_millis(50));
    }
    match SAVE_READY.load(Relaxed) {
        true => tracing::info!("save sync: the host's save is in place; the game reads it now"),
        false => tracing::warn!("save sync: no save from the host; the game reads the saves it has"),
    }
}

// -------------------------------------------------------------------------------------------------------
// State for the net thread and the overlay

/// Client session started: slot files now come from the host's world.
pub fn enter_host_world() {
    if !HOST_WORLD.swap(true, Relaxed) {
        tracing::info!("save redirection: host's world on (your own saves are protected until the game restarts)");
    }
}

/// The game's next save read waits for the host's save (a "Join" at startup).
pub fn hold_next_read(on: bool) {
    HOLD_RELEASED.store(!on, Relaxed);
    HOLD_NEXT.store(on, Relaxed);
}

/// Client session ended: if nothing was loaded from the host, the player's own saves are usable again.
pub fn session_ended() {
    if origin() != Origin::Host && HOST_WORLD.swap(false, Relaxed) {
        tracing::info!("save redirection: off (nothing was loaded from the host)");
    }
    hold_next_read(false);
}

pub fn host_world() -> bool {
    HOST_WORLD.load(Relaxed)
}

/// The game has read its save list (after that, a newly arrived save only shows after a restart).
pub fn listed() -> bool {
    FIRST_READ.get().is_some()
}

pub fn origin() -> Origin {
    match ORIGIN.load(Relaxed) {
        1 => Origin::Local,
        2 => Origin::Host,
        _ => Origin::None,
    }
}

/// The host's save has been written to `host_save_path` for `slot`.
pub fn host_save_written(slot: u32) {
    HOST_SLOT.store(slot, Relaxed);
    SAVE_READY.store(true, Relaxed);
    let _ = std::fs::write(data_dir().join("host_slot.txt"), slot.to_string());
}

/// The slot of our own the game loaded last, if a load was seen.
pub fn loaded_slot() -> Option<u32> {
    let s = LOADED_SLOT.load(Relaxed);
    (s < SLOTS).then_some(s)
}

/// (the host's save arrived this run, its slot).
pub fn host_save() -> (bool, Option<u32>) {
    let s = HOST_SLOT.load(Relaxed);
    (SAVE_READY.load(Relaxed), (s < SLOTS).then_some(s))
}

// -------------------------------------------------------------------------------------------------------
// "Join" restart

fn autojoin_path() -> PathBuf {
    data_dir().join("autojoin.txt")
}

/// Startup: the join code a "Join" restart left, consumed once.
pub fn take_autojoin() -> Option<String> {
    let p = autojoin_path();
    let code = std::fs::read_to_string(&p).ok()?;
    let _ = std::fs::remove_file(&p);
    let code = code.trim().to_string();
    (!code.is_empty() && code.len() < 4096).then_some(code)
}

/// Restarts the game, which then joins `code` and loads the host's world. Unsaved progress is lost.
pub fn restart_into_host_world(code: &str) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    ensure_dir();
    std::fs::write(autojoin_path(), code.trim()).map_err(|e| format!("could not write {}: {e}", autojoin_path().display()))?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    // The game allows one copy at a time: start the new one after this one is gone (~3 s), launched directly
    // with the Steam app id like `tools/two-instances/launch-two.ps1`.
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::process::Command::new("cmd")
        .raw_arg(format!("/C ping -n 4 127.0.0.1 >nul & start \"\" \"{}\"", exe.display()))
        .env("SteamAppId", "524220")
        .current_dir(crate::config::game_dir())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("could not start the relaunch: {e}"))?;
    tracing::info!("restarting the game to join the host's world");
    // SAFETY: ends our own process; nothing runs after this.
    unsafe {
        let _ = windows::Win32::System::Threading::TerminateProcess(windows::Win32::System::Threading::GetCurrentProcess(), 0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::slot_of;

    #[test]
    fn slot_names() {
        assert_eq!(slot_of(br"C:\Users\x\Documents\My Games\NieR_Automata\SlotData_1.dat"), Some(1));
        assert_eq!(slot_of(b"slotdata_0.DAT"), Some(0));
        assert_eq!(slot_of(br"C:\x\SlotData_1.dat.nier_coop.bak"), None);
        assert_eq!(slot_of(br"C:\x\GameData.dat"), None);
        assert_eq!(slot_of(br"C:\x\SlotData_12.dat"), None);
    }
}
