//! Save transfer (port of nier_coop `net/save.rs`, on a QUIC stream instead of UDP chunks). The host sends its
//! most recently written slot once per session and again whenever it changes, each time on its own
//! unidirectional stream (CLAUDE.md Phase 4: bulk transfers on their own stream, so they never block events).
//! The client checks it and writes it to `hooks::savefile::host_save_path`, never over its own slots.
//!
//! Stream layout: `magic u32, slot u32, crc32 u32, len u32` (little-endian), then `len` bytes. Untrusted
//! (CLAUDE.md rule 4): the length must be exactly a save's, the slot in range, and the CRC must match.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::time::{Duration, SystemTime};

use crate::hooks::savefile::{self, SAVE_SIZE, SLOTS};
use crate::sdk::crc32;

pub const MAGIC: u32 = u32::from_le_bytes(*b"NOSV");
pub const HEADER: usize = 16;
/// Steam IDs of individual accounts have this high word (bytes 4..8 little-endian).
const STEAM_ID_HIGH: [u8; 4] = [0x01, 0x00, 0x10, 0x01];

/// `Documents\My Games\NieR_Automata`, through the shell so a OneDrive-moved Documents folder is found
/// (nier_coop `save_dir`).
pub fn save_dir() -> Option<PathBuf> {
    use windows::Win32::System::Com::CoTaskMemFree;
    use windows::Win32::UI::Shell::{FOLDERID_Documents, KF_FLAG_DEFAULT, SHGetKnownFolderPath};
    // SAFETY: shell call returning a string we free with CoTaskMemFree.
    let docs = unsafe {
        let p = SHGetKnownFolderPath(&FOLDERID_Documents, KF_FLAG_DEFAULT, None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s?
    };
    let dir = PathBuf::from(docs).join(r"My Games\NieR_Automata");
    dir.is_dir().then_some(dir)
}

fn slot_path(dir: &Path, slot: u32) -> PathBuf {
    dir.join(format!("SlotData_{slot}.dat"))
}

/// Host: the slot chosen to share when hosting (`u32::MAX`: none chosen, share the newest save once there is
/// one, for a new game). Kept across sessions, so a client that reconnects gets the same world.
static SHARED: AtomicU32 = AtomicU32::new(u32::MAX);

pub fn set_shared(slot: Option<u32>) {
    SHARED.store(slot.filter(|&s| s < SLOTS).unwrap_or(u32::MAX), Relaxed);
}

pub fn shared() -> Option<u32> {
    let s = SHARED.load(Relaxed);
    (s < SLOTS).then_some(s)
}

/// One of the host's save slots, for the world picker.
#[derive(Clone, Debug)]
pub struct SlotInfo {
    pub slot: u32,
    pub name: String,
    pub saved: SystemTime,
}

/// The host's saves (valid ones only), read from their headers.
pub fn slot_infos(dir: &Path) -> Vec<SlotInfo> {
    (0..SLOTS)
        .filter_map(|slot| {
            let p = slot_path(dir, slot);
            let m = std::fs::metadata(&p).ok().filter(|m| m.len() == SAVE_SIZE as u64)?;
            let mut head = [0u8; NAME_END];
            std::io::Read::read_exact(&mut std::fs::File::open(&p).ok()?, &mut head).ok()?;
            Some(SlotInfo { slot, name: player_name(&head).unwrap_or_default(), saved: m.modified().ok()? })
        })
        .collect()
}

/// Host: the save to send: the slot the game loaded (`loaded`), or without one the newest save. Not when it is
/// the one sent last (`last` = slot, crc) or was written under a second ago (the game may still be writing it).
/// Returns (slot, crc, data).
pub fn to_send(dir: &Path, loaded: Option<u32>, last: Option<(u32, u32)>) -> Option<(u32, u32, Vec<u8>)> {
    let valid = |s: u32| {
        let m = std::fs::metadata(slot_path(dir, s)).ok().filter(|m| m.len() == SAVE_SIZE as u64)?;
        Some((s, m.modified().ok()?))
    };
    let (slot, mtime) = match loaded.and_then(valid) {
        Some(v) => v,
        None => (0..SLOTS).filter_map(valid).max_by_key(|&(_, t)| t)?,
    };
    if SystemTime::now().duration_since(mtime).is_ok_and(|e| e < Duration::from_secs(1)) {
        return None;
    }
    let data = std::fs::read(slot_path(dir, slot)).ok()?;
    if data.len() != SAVE_SIZE {
        return None;
    }
    let crc = crc32(&data);
    (last != Some((slot, crc))).then_some((slot, crc, data))
}

pub fn header(slot: u32, crc: u32, len: usize) -> [u8; HEADER] {
    let mut h = [0u8; HEADER];
    for (i, v) in [MAGIC, slot, crc, len as u32].into_iter().enumerate() {
        h[i * 4..i * 4 + 4].copy_from_slice(&v.to_le_bytes());
    }
    h
}

/// (slot, crc) from a received header, if it announces a valid save.
pub fn parse_header(h: &[u8; HEADER]) -> Result<(u32, u32), String> {
    let w = |i: usize| u32::from_le_bytes(h[i * 4..i * 4 + 4].try_into().unwrap());
    if w(0) != MAGIC {
        return Err("not a save transfer".into());
    }
    if w(1) >= SLOTS {
        return Err(format!("bad slot {}", w(1)));
    }
    if w(3) as usize != SAVE_SIZE {
        return Err(format!("bad save size {}", w(3)));
    }
    Ok((w(1), w(2)))
}

/// This player's SteamID64, from the first 8 bytes of their own GameData.dat (nier_coop).
fn own_steam_id(dir: &Path) -> Option<[u8; 8]> {
    let g = std::fs::read(dir.join("GameData.dat")).ok()?;
    let id: [u8; 8] = g.get(..8)?.try_into().ok()?;
    (id[4..] == STEAM_ID_HIGH).then_some(id)
}

/// Each slot stores its owner's SteamID64 at byte 4; the game shows a slot with another account's ID as empty,
/// so the host's save gets this player's ID (nier_coop `set_owner`).
pub fn set_owner(data: &mut [u8], id: [u8; 8]) {
    if data.get(8..12) == Some(&STEAM_ID_HIGH[..]) {
        data[4..12].copy_from_slice(&id);
    }
}

/// The player name in a slot's header: UTF-16, NUL-padded, from here up to the save's text tree at 0x7C (seen in
/// the Steam saves of build 7788: "Rainy" at 0x34). It is the only text in the slot header.
const NAME_AT: usize = 0x34;
const NAME_END: usize = 0x7C;
pub const ONLINE_LABEL: &str = "[online] ";

/// Prefixes the slot's player name with `ONLINE_LABEL`, so the client can tell the host's world from its own in
/// the save list. Truncated to fit, always NUL-terminated; left alone if already labelled or not a name.
/// The slot's player name, if the header holds a readable one.
pub fn player_name(data: &[u8]) -> Option<String> {
    let field = data.get(NAME_AT..NAME_END)?;
    let units: Vec<u16> = field.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
    let len = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16(&units[..len]).ok().filter(|n| !n.chars().any(char::is_control))
}

pub fn label_online(data: &mut [u8]) {
    let Some(name) = player_name(data) else { return };
    if name.starts_with(ONLINE_LABEL.trim_end()) {
        return;
    }
    let units = (NAME_END - NAME_AT) / 2;
    let mut out: Vec<u16> = format!("{ONLINE_LABEL}{name}").encode_utf16().collect();
    out.truncate(units - 1); // room for the terminator
    out.resize(units, 0);
    for (c, u) in data[NAME_AT..NAME_END].chunks_exact_mut(2).zip(out) {
        c.copy_from_slice(&u.to_le_bytes());
    }
}

/// Client: checks a received save and writes it as the host's world. `dir` is our own save folder (only read,
/// for our Steam ID).
pub fn accept(slot: u32, crc: u32, mut data: Vec<u8>, dir: Option<&Path>) -> Result<(), String> {
    if data.len() != SAVE_SIZE || slot >= SLOTS {
        return Err("bad save".into());
    }
    if crc32(&data) != crc {
        return Err("save checksum mismatch".into());
    }
    match dir.and_then(own_steam_id) {
        Some(id) => set_owner(&mut data, id),
        None => tracing::warn!("save sync: your Steam ID was not found (no GameData.dat?); the slot may show as empty"),
    }
    if crate::config::get().online_save_label {
        label_online(&mut data);
    }
    let path = savefile::host_save_path();
    let _ = std::fs::create_dir_all(savefile::data_dir());
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &data).and_then(|()| std::fs::rename(&tmp, &path)).map_err(|e| format!("could not write {}: {e}", path.display()))?;
    savefile::host_save_written(slot);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip_and_rejects() {
        assert_eq!(parse_header(&header(2, 0xDEAD_BEEF, SAVE_SIZE)), Ok((2, 0xDEAD_BEEF)));
        assert!(parse_header(&header(3, 0, SAVE_SIZE)).is_err());
        assert!(parse_header(&header(0, 0, SAVE_SIZE + 1)).is_err());
        let mut h = header(0, 0, SAVE_SIZE);
        h[0] ^= 1;
        assert!(parse_header(&h).is_err());
    }

    #[test]
    fn owner_id_swapped() {
        let host = [0, 0, 0, 0, 0x68, 0x8F, 0x66, 0x5B, 1, 0, 0x10, 1, 0x54, 0x59];
        let mine = [0x11, 0x22, 0x33, 0x44, 1, 0, 0x10, 1];
        let mut d = host.to_vec();
        set_owner(&mut d, mine);
        assert_eq!(d, [0, 0, 0, 0, 0x11, 0x22, 0x33, 0x44, 1, 0, 0x10, 1, 0x54, 0x59]);
        let mut odd = vec![0u8; 14];
        set_owner(&mut odd, mine);
        assert_eq!(odd, vec![0u8; 14]);
    }

    fn with_name(name: &str) -> Vec<u8> {
        let mut d = vec![0u8; 0x90];
        for (i, u) in name.encode_utf16().enumerate() {
            d[NAME_AT + 2 * i..NAME_AT + 2 * i + 2].copy_from_slice(&u.to_le_bytes());
        }
        d[NAME_END..NAME_END + 4].copy_from_slice(b"vers");
        d
    }

    fn name_of(d: &[u8]) -> String {
        let u: Vec<u16> = d[NAME_AT..NAME_END].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        String::from_utf16_lossy(&u[..u.iter().position(|&x| x == 0).unwrap()])
    }

    #[test]
    fn online_label() {
        let mut d = with_name("Rainy");
        label_online(&mut d);
        assert_eq!(name_of(&d), "[online] Rainy");
        label_online(&mut d); // once only
        assert_eq!(name_of(&d), "[online] Rainy");
        assert_eq!(&d[NAME_END..NAME_END + 4], b"vers"); // the text tree after it is untouched
        // A long name is cut to fit and stays terminated.
        let mut long = with_name(&"x".repeat(35));
        label_online(&mut long);
        assert_eq!(name_of(&long).chars().count(), 35);
        assert!(name_of(&long).starts_with("[online] x"));
        assert_eq!(&long[NAME_END..NAME_END + 4], b"vers");
    }

    #[test]
    fn newest_slot_picked() {
        let dir = std::env::temp_dir().join(format!("nier_online_save_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(slot_path(&dir, 0), vec![1u8; SAVE_SIZE]).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(slot_path(&dir, 2), vec![2u8; SAVE_SIZE]).unwrap();
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(slot_path(&dir, 1), b"not a save").unwrap(); // newest, but the wrong size: skipped
        std::thread::sleep(Duration::from_millis(1100));
        let (slot, crc, data) = to_send(&dir, None, None).unwrap();
        assert_eq!((slot, data[0]), (2, 2));
        assert!(to_send(&dir, None, Some((slot, crc))).is_none());
        // The slot the host loaded wins over a newer one; a loaded slot that is not a save falls back.
        assert_eq!(to_send(&dir, Some(0), None).map(|t| t.0), Some(0));
        assert_eq!(to_send(&dir, Some(1), None).map(|t| t.0), Some(2));
        // The world picker lists only real saves.
        assert_eq!(slot_infos(&dir).iter().map(|s| s.slot).collect::<Vec<_>>(), [0, 2]);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
