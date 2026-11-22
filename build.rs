//! Stamps the build: `NIER_ONLINE_COMMIT` (git short hash, "-dirty" with uncommitted changes, "nogit" outside a
//! checkout) goes into the mod version both players must match; `NIER_ONLINE_BUILT` (UTC time) is shown in the
//! menu and log so you can tell two builds of the same commit apart. No `rerun-if-changed` lines on purpose:
//! Cargo then reruns this whenever any file in the package changes, which keeps both values current.

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// UTC `YYYY-MM-DD HH:MM` from Unix seconds (days-to-civil, Howard Hinnant's algorithm).
fn utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + (m <= 2) as i64;
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02} UTC", rem / 3600, rem % 3600 / 60)
}

fn main() {
    let commit = match git(&["rev-parse", "--short=7", "HEAD"]) {
        Some(hash) => {
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"]).is_some_and(|s| !s.is_empty());
            if dirty { format!("{hash}-dirty") } else { hash }
        }
        None => "nogit".into(),
    };
    let secs = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    println!("cargo:rustc-env=NIER_ONLINE_COMMIT={commit}");
    println!("cargo:rustc-env=NIER_ONLINE_BUILT={}", utc(secs));
}
