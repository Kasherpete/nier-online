//! Settings in `nier_online.toml` next to the game exe (Steam's working directory is not the game folder).
//! A missing file means defaults; the overlay's Save button writes it.

use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Key that toggles the overlay: a virtual-key name such as "Insert", "F8" or "Home".
    pub overlay_key: String,
    /// Shown to the other player.
    pub player_name: String,
    /// The host owns enemies and the client mirrors them. The host's setting decides. On by default since
    /// 2026-11-22 (verified in-game: enemies spawn and stay in sync).
    pub enemy_sync: bool,
    /// Relay server URL. Empty = iroh's default public relays (n0).
    pub relay_url: String,
    /// Log level: "error", "warn", "info", "debug" or "trace".
    pub log_level: String,
    /// Debug: draw the debug panel (entity explorer).
    pub debug_panel: bool,
    /// Client: replay host enemy animations even for (enemy class, animation) pairs this game has not started
    /// itself yet. On by default (2026-11-22: without it, enemies slide without attacking until the allowlist
    /// fills, and no crash was seen with it on). Turn off if enemy fights crash: an id the engine has never
    /// used for that class could.
    pub enemy_replay_unverified: bool,
    /// Host: enemies and bosses target whichever player is nearer, not only the host's character (see
    /// `sync::aggro`). Added 2026-11-22, not yet verified in-game; turn off if enemies act strangely.
    pub enemy_target_client: bool,
    /// When the other player's character dies, yours dies too, so both games reload their last checkpoint.
    /// Each player's own setting decides whether they follow the other's death.
    pub shared_death: bool,
    /// When the other player starts a cutscene, warp to them and play it too. Each player's own setting
    /// decides whether they follow.
    pub cutscene_sync: bool,
    /// Client: the host's save shows as "[online] <name>" in the save list (its player name is changed in the
    /// client's copy only). Turn off if that slot shows as damaged.
    pub online_save_label: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            overlay_key: "Insert".into(),
            player_name: "Player".into(),
            enemy_sync: true,
            relay_url: String::new(),
            log_level: "info".into(),
            debug_panel: false,
            enemy_replay_unverified: true,
            enemy_target_client: true,
            shared_death: true,
            cutscene_sync: true,
            online_save_label: true,
        }
    }
}

pub fn game_dir() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(Into::into)).unwrap_or_else(|| ".".into())
}

fn path() -> PathBuf {
    game_dir().join("nier_online.toml")
}

/// Loaded at startup.
pub static CONFIG: LazyLock<RwLock<Config>> = LazyLock::new(|| RwLock::new(load()));

fn load() -> Config {
    match std::fs::read_to_string(path()) {
        Ok(text) => toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("nier_online.toml is invalid ({e}); using defaults");
            Config::default()
        }),
        Err(_) => Config::default(),
    }
}

pub fn get() -> Config {
    CONFIG.read().map(|c| c.clone()).unwrap_or_default()
}

pub fn set(c: Config) -> std::io::Result<()> {
    let text = toml::to_string_pretty(&c).map_err(std::io::Error::other)?;
    std::fs::write(path(), text)?;
    if let Ok(mut g) = CONFIG.write() {
        *g = c;
    }
    Ok(())
}

/// Virtual-key code for a key name ("Insert", "F1".."F12", "Home", "End", "Delete", "PageUp", "PageDown",
/// or a single letter or digit).
pub fn vk_from_name(name: &str) -> Option<u16> {
    let n = name.trim().to_ascii_uppercase();
    Some(match n.as_str() {
        "INSERT" | "INS" => 0x2D,
        "DELETE" | "DEL" => 0x2E,
        "HOME" => 0x24,
        "END" => 0x23,
        "PAGEUP" | "PGUP" => 0x21,
        "PAGEDOWN" | "PGDN" => 0x22,
        _ if n.len() >= 2 && n.starts_with('F') => {
            let i: u16 = n[1..].parse().ok()?;
            if !(1..=24).contains(&i) {
                return None;
            }
            0x70 + i - 1
        }
        _ if n.len() == 1 && n.as_bytes()[0].is_ascii_alphanumeric() => n.as_bytes()[0] as u16,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_roundtrip_and_partial_files() {
        let c = Config::default();
        assert_eq!(toml::from_str::<Config>(&toml::to_string(&c).unwrap()).unwrap(), c);
        let partial: Config = toml::from_str("player_name = \"9S\"").unwrap();
        assert_eq!(partial.player_name, "9S");
        assert_eq!(partial.overlay_key, "Insert");
    }

    #[test]
    fn key_names() {
        assert_eq!(vk_from_name("Insert"), Some(0x2D));
        assert_eq!(vk_from_name("f8"), Some(0x77));
        assert_eq!(vk_from_name("K"), Some(b'K' as u16));
        assert_eq!(vk_from_name("F99"), None);
        assert_eq!(vk_from_name("nope"), None);
    }
}
