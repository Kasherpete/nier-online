//! Crash handler (design of AutomataMP `ExceptionHandler.cpp`): on an unhandled exception, write a minidump and
//! the last log lines to `<game folder>\nier_online\crash-<time>.dmp` / `.log`, then let any handler that was
//! installed before ours (the game's, Steam's) run as usual.

use std::io::Write;
use std::os::windows::io::AsRawHandle;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Diagnostics::Debug::{
    EXCEPTION_POINTERS, LPTOP_LEVEL_EXCEPTION_FILTER, MINIDUMP_EXCEPTION_INFORMATION, MiniDumpWithIndirectlyReferencedMemory,
    MiniDumpWriteDump, SetUnhandledExceptionFilter,
};
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, GetCurrentThreadId};

const EXCEPTION_CONTINUE_SEARCH: i32 = 0;

static PREVIOUS: OnceLock<LPTOP_LEVEL_EXCEPTION_FILTER> = OnceLock::new();
static WRITING: AtomicBool = AtomicBool::new(false);

pub fn install() {
    // SAFETY: plain Win32 call; `filter` has the required signature.
    let prev = unsafe { SetUnhandledExceptionFilter(Some(filter)) };
    let _ = PREVIOUS.set(prev);
    tracing::info!("crash handler installed (dumps go to {})", crate::hooks::savefile::data_dir().display());
}

unsafe extern "system" fn filter(info: *const EXCEPTION_POINTERS) -> i32 {
    // Only the first crashing thread writes; a crash while writing must not recurse.
    if !WRITING.swap(true, Relaxed) {
        write_report(info);
    }
    match PREVIOUS.get().copied().flatten() {
        // SAFETY: the filter that was installed before ours, with the same argument.
        Some(prev) => unsafe { prev(info) },
        None => EXCEPTION_CONTINUE_SEARCH,
    }
}

fn write_report(info: *const EXCEPTION_POINTERS) {
    let dir = crate::hooks::savefile::data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    // SAFETY: the exception record pointer comes from the OS and is null-checked.
    let (code, at) = unsafe { info.as_ref().and_then(|i| i.ExceptionRecord.as_ref()) }
        .map_or((0, 0), |r| (r.ExceptionCode.0 as u32, r.ExceptionAddress as usize));
    let base = crate::hooks::GAME_BASE.load(Relaxed);
    if let Ok(mut f) = std::fs::File::create(dir.join(format!("crash-{stamp}.log"))) {
        let _ = writeln!(f, "nier-online {} (built {})", crate::net::MOD_VERSION, crate::net::BUILT);
        let rva = if base != 0 && at >= base { format!(" (game+{:#x})", at - base) } else { String::new() };
        let _ = writeln!(f, "exception {code:#010x} at {at:#x}{rva}\n\nlast log lines:");
        // try_lock: the crashing thread may hold the log lock.
        if let Ok(r) = crate::log::RING.try_lock() {
            for l in r.iter() {
                let _ = writeln!(f, "{l}");
            }
        }
    }
    if let Ok(f) = std::fs::File::create(dir.join(format!("crash-{stamp}.dmp"))) {
        let ex = MINIDUMP_EXCEPTION_INFORMATION {
            // SAFETY: plain Win32 call.
            ThreadId: unsafe { GetCurrentThreadId() },
            ExceptionPointers: info as *mut _,
            ClientPointers: false.into(),
        };
        // SAFETY: our own process, an open file handle, and the OS's exception pointers.
        let _ = unsafe {
            MiniDumpWriteDump(
                GetCurrentProcess(),
                GetCurrentProcessId(),
                HANDLE(f.as_raw_handle()),
                MiniDumpWithIndirectlyReferencedMemory,
                Some(&ex),
                None,
                None,
            )
        };
    }
}
