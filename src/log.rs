//! Logging: `nier_online.log` next to the game exe (truncated each run; a second copy of the game running at
//! the same time writes `nier_online.2.log`), a console window in debug builds,
//! and the last few hundred lines kept in memory for the overlay's log panel.

use std::collections::VecDeque;
use std::io::Write;
use std::sync::{LazyLock, Mutex};

use tracing::Level;
use tracing_subscriber::filter::Targets;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const RING_LINES: usize = 300;

/// Recent log lines, newest last.
pub static RING: LazyLock<Mutex<VecDeque<String>>> = LazyLock::new(|| Mutex::new(VecDeque::new()));

#[derive(Clone, Copy)]
struct RingWriter;

impl Write for RingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if let Ok(mut r) = RING.lock() {
            for line in String::from_utf8_lossy(buf).lines().filter(|l| !l.is_empty()) {
                r.push_back(line.to_string());
            }
            while r.len() > RING_LINES {
                r.pop_front();
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for RingWriter {
    type Writer = RingWriter;
    fn make_writer(&'a self) -> Self::Writer {
        *self
    }
}

fn level(name: &str) -> Level {
    name.trim().parse().unwrap_or(Level::INFO)
}

/// Log file name for this game process: `nier_online.log` for the first copy of the game, `nier_online.2.log`
/// for a second one running at the same time (the two-instance test), so they don't write over each other.
/// Each copy holds a named mutex for its slot until it exits.
fn file_name() -> String {
    use windows::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows::Win32::System::Threading::CreateMutexW;
    use windows::core::HSTRING;
    for n in 1..=4 {
        let name = HSTRING::from(format!("Local\\nier_online_log_{n}"));
        // SAFETY: plain Win32 calls; the handle is deliberately kept open for the life of the process.
        let taken = unsafe {
            match CreateMutexW(None, false, &name) {
                Ok(h) => {
                    let exists = GetLastError() == ERROR_ALREADY_EXISTS;
                    if exists {
                        let _ = windows::Win32::Foundation::CloseHandle(h);
                    }
                    exists
                }
                Err(_) => true,
            }
        };
        if !taken {
            return if n == 1 { "nier_online.log".into() } else { format!("nier_online.{n}.log") };
        }
    }
    format!("nier_online.{}.log", std::process::id())
}

/// Returns the log file name.
pub fn init(level_name: &str) -> String {
    // Our own crate at the configured level; libraries (iroh, quinn) only at warn, since they are chatty. At
    // "debug" or "trace", iroh too: its path selection (direct vs relay, hole punching) is how a failed join is
    // diagnosed.
    let ours = level(level_name);
    let iroh = if ours >= Level::DEBUG { Level::DEBUG } else { Level::WARN };
    let filter = Targets::new()
        .with_target("nier_online", ours)
        .with_target("iroh", iroh)
        .with_target("noq", iroh)
        .with_default(Level::WARN);

    let name = file_name();
    let file = std::fs::File::create(crate::config::game_dir().join(&name)).ok().map(|f| {
        let (w, guard) = tracing_appender::non_blocking(f);
        std::mem::forget(guard); // flushes on drop; we live until the process exits
        tracing_subscriber::fmt::layer().with_ansi(false).with_writer(w)
    });
    let ring = tracing_subscriber::fmt::layer().with_ansi(false).without_time().with_target(false).with_writer(RingWriter);
    let console = cfg!(debug_assertions).then(|| {
        // SAFETY: plain Win32 call; failure (a console already exists) is harmless.
        unsafe {
            let _ = windows::Win32::System::Console::AllocConsole();
        }
        tracing_subscriber::fmt::layer().with_ansi(false).with_writer(std::io::stdout)
    });
    let _ = tracing_subscriber::registry().with(filter).with(file).with(ring).with(console).try_init();
    name
}
