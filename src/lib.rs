//! nier-online: peer-to-peer two-player co-op for NieR:Automata.
//!
//! Loaded by the game as `dinput8.dll` (see `proxy`). `DllMain` only starts a worker thread, because the
//! loader lock is held inside it. The worker resolves every game address, installs the hooks, and starts
//! the network thread and the overlay.

pub mod addr;
pub mod config;
pub mod hooks;
pub mod log;
pub mod net;
pub mod proxy;
pub mod scan;
pub mod sdk;
pub mod state;
pub mod sync;
pub mod ui;

use std::ffi::c_void;

use windows::Win32::Foundation::{HINSTANCE, HMODULE};
use windows::Win32::System::LibraryLoader::{DisableThreadLibraryCalls, GetModuleHandleW};
use windows::Win32::System::Memory::{MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_GUARD, PAGE_NOACCESS, VirtualQuery};
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::core::{BOOL, PCWSTR};

#[unsafe(no_mangle)]
extern "system" fn DllMain(module: HINSTANCE, reason: u32, _reserved: *mut c_void) -> BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // SAFETY: plain Win32 call on our own module handle.
        unsafe {
            let _ = DisableThreadLibraryCalls(HMODULE(module.0));
        }
        let m = module.0 as usize;
        // The loader lock is held here: hand off and return. The DLL is never unloaded (hooks stay
        // installed), so nothing is done on detach.
        let _ = std::thread::Builder::new().name("nier_online init".into()).spawn(move || worker(HINSTANCE(m as *mut c_void)));
    }
    BOOL(1)
}

/// The game module as a byte slice, after checking every page is committed and readable.
fn live_image() -> Result<(&'static [u8], usize), String> {
    // SAFETY: Win32 queries on our own process; the slice is only formed after every page checks out.
    unsafe {
        let h = GetModuleHandleW(PCWSTR::null()).map_err(|e| format!("GetModuleHandle: {e}"))?;
        let mut info = MODULEINFO::default();
        GetModuleInformation(GetCurrentProcess(), h, &mut info, size_of::<MODULEINFO>() as u32)
            .map_err(|e| format!("GetModuleInformation: {e}"))?;
        let base = info.lpBaseOfDll as usize;
        let size = info.SizeOfImage as usize;
        let mut at = base;
        while at < base + size {
            let mut mbi = MEMORY_BASIC_INFORMATION::default();
            if VirtualQuery(Some(at as *const c_void), &mut mbi, size_of::<MEMORY_BASIC_INFORMATION>()) == 0 {
                return Err(format!("VirtualQuery failed at {at:#x}"));
            }
            if mbi.State != MEM_COMMIT || mbi.Protect.contains(PAGE_NOACCESS) || mbi.Protect.contains(PAGE_GUARD) {
                return Err(format!("game module page at {at:#x} is not readable"));
            }
            at = mbi.BaseAddress as usize + mbi.RegionSize;
        }
        Ok((std::slice::from_raw_parts(base as *const u8, size), base))
    }
}

fn worker(module: HINSTANCE) {
    let cfg = config::get();
    let log_file = log::init(&cfg.log_level);
    tracing::info!("nier-online {} (built {}) loaded, logging to {log_file}", net::MOD_VERSION, net::BUILT);

    let (bytes, base) = match live_image() {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("cannot read the game module: {e}. The mod is disabled.");
            return;
        }
    };
    let resolved = scan::Image::new(bytes, Some(base as u64)).and_then(|img| addr::resolve(&img, base).map(|a| (a, img)));
    let (addresses, img) = match resolved {
        Ok(v) => v,
        Err(e) => {
            tracing::error!("address lookup failed: {e}");
            tracing::error!("The game was probably updated. No hooks were installed; the game runs unmodded.");
            return;
        }
    };
    tracing::info!("game build {:08x}, all {} addresses resolved", addresses.build, 18);
    let build = addresses.build;
    if let Err(e) = hooks::install(addresses, img) {
        // Hooks are installed in an order where a failure leaves the game consistent (the tick goes last).
        tracing::error!("hook install failed: {e}");
        return;
    }
    net::spawn(build);
    ui::start(module);
}
