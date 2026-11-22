//! Windows clipboard for ImGui. hudhook does not install a clipboard backend, so without this
//! `set_clipboard_text`, `clipboard_text` and Ctrl+C / Ctrl+V in text fields silently do nothing.

use hudhook::imgui::ClipboardBackend;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

pub struct WinClipboard;

/// Opens the clipboard, retrying briefly: another program may hold it for a moment.
fn open() -> bool {
    for _ in 0..10 {
        // SAFETY: plain Win32 call; no owner window is needed.
        if unsafe { OpenClipboard(None) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    false
}

impl ClipboardBackend for WinClipboard {
    fn get(&mut self) -> Option<String> {
        if !open() {
            return None;
        }
        // SAFETY: the clipboard is open; the handle is locked while read and the text is NUL-terminated
        // UTF-16, as CF_UNICODETEXT guarantees.
        let text = unsafe {
            GetClipboardData(CF_UNICODETEXT.0 as u32).ok().and_then(|h| {
                let mem = HGLOBAL(h.0);
                let p = GlobalLock(mem) as *const u16;
                if p.is_null() {
                    return None;
                }
                let mut n = 0;
                while *p.add(n) != 0 {
                    n += 1;
                }
                let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                let _ = GlobalUnlock(mem);
                Some(s)
            })
        };
        // SAFETY: we opened it above.
        let _ = unsafe { CloseClipboard() };
        text
    }

    fn set(&mut self, value: &str) {
        if !open() {
            return;
        }
        let wide: Vec<u16> = value.encode_utf16().chain([0]).collect();
        // SAFETY: the clipboard is open; we copy into a freshly allocated movable block, and ownership passes
        // to the system only if SetClipboardData succeeds (otherwise we free it).
        unsafe {
            let _ = EmptyClipboard();
            if let Ok(mem) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
                let p = GlobalLock(mem) as *mut u16;
                if !p.is_null() {
                    std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
                    let _ = GlobalUnlock(mem);
                    if SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(mem.0))).is_err() {
                        let _ = GlobalFree(Some(mem));
                    }
                } else {
                    let _ = GlobalFree(Some(mem));
                }
            }
            let _ = CloseClipboard();
        }
    }
}
