//! The ImGui overlay, drawn by hudhook from the DX11 swap chain (the approach of AutomataMP
//! `D3D11Hook.cpp` / `WindowsMessageHook.cpp`). It never touches game memory: it reads the status structs
//! in `crate::state` and sends commands through queues.

mod chat;
mod clipboard;
mod cursor;

use std::sync::atomic::Ordering::Relaxed;

use hudhook::imgui::{Condition, Context, Io, Ui};
use hudhook::{ImguiRenderLoop, MessageFilter, RenderContext};
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

use crate::config::{self, Config};
use crate::hooks::savefile;
use crate::state::{self, NetCommand, Q};

const GREEN: [f32; 4] = [0.4, 0.9, 0.4, 1.0];
const RED: [f32; 4] = [1.0, 0.45, 0.4, 1.0];
const GREY: [f32; 4] = [0.7, 0.7, 0.7, 1.0];

pub fn start(module: HINSTANCE) {
    cursor::install();
    let overlay = Overlay::new();
    let hooks = hudhook::Hudhook::builder().with::<hudhook::hooks::dx11::ImguiDx11Hooks>(overlay).with_hmodule(module).build();
    match hooks.apply() {
        Ok(()) => tracing::info!("overlay installed (toggle with {})", config::get().overlay_key),
        Err(e) => tracing::error!("overlay could not hook DirectX 11: {e:?}"),
    }
}

struct Overlay {
    visible: bool,
    key_was_down: bool,
    vk: u16,
    join_code: String,
    chat: String,
    chat_box: chat::ChatBox,
    /// Game input was blocked last frame (menu or chat box open).
    captured: bool,
    cfg: Config,
    save_msg: Option<(String, bool)>,
    entity_filter: String,
    /// "Join" needs a restart: the code to join after it (shows the confirmation).
    restart_code: Option<String>,
    restart_error: Option<String>,
    /// "Host" was pressed: the saves to pick the shared world from.
    world_picker: Option<Vec<crate::net::save::SlotInfo>>,
}

impl Overlay {
    fn new() -> Self {
        let cfg = config::get();
        Self {
            visible: false,
            key_was_down: false,
            vk: config::vk_from_name(&cfg.overlay_key).unwrap_or(0x2D),
            join_code: String::new(),
            chat: String::new(),
            chat_box: chat::ChatBox::new(),
            captured: false,
            cfg,
            save_msg: None,
            entity_filter: String::new(),
            restart_code: None,
            restart_error: None,
            world_picker: None,
        }
    }

    /// Edge-detects the menu and chat keys, only while this game window has focus (two copies can run side
    /// by side), and blocks game input while either is open.
    fn poll_toggle(&mut self) {
        // SAFETY: plain Win32 queries.
        let focused = unsafe {
            let mut pid = 0;
            GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid));
            pid == GetCurrentProcessId()
        };
        let down = focused && unsafe { GetAsyncKeyState(self.vk as i32) } as u16 & 0x8000 != 0;
        if down && !self.key_was_down {
            self.visible = !self.visible;
            state::DEBUG_LIST_WANTED.store(self.visible && self.cfg.debug_panel, Relaxed);
        }
        self.key_was_down = down;
        self.chat_box.poll(focused, self.visible);
        let captured = self.capturing();
        if captured && !self.captured {
            cursor::release();
        }
        self.captured = captured;
        state::OVERLAY_VISIBLE.store(captured, Relaxed);
    }

    fn capturing(&self) -> bool {
        self.visible || self.chat_box.captures()
    }
}

/// "5 min ago" style age of a save.
fn ago(t: std::time::SystemTime) -> String {
    let s = t.elapsed().map_or(0, |d| d.as_secs());
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => format!("{} days ago", s / 86_400),
    }
}

/// Host: which world is shared, and a warning when the host is playing a different slot.
fn shared_world_line(ui: &Ui, info: &state::OverlayInfo) {
    let shared = crate::net::save::shared();
    match shared {
        Some(s) => ui.text_colored(GREEN, format!("Sharing your world in slot {}.", s + 1)),
        None => ui.text_colored(GREY, "Sharing your newest save (none yet for a new game)."),
    }
    if let (Some(s), Some(l), true) = (shared, savefile::loaded_slot(), info.local_player)
        && s != l
    {
        ui.text_colored(RED, format!("You are playing slot {}: load slot {} so both worlds match.", l + 1, s + 1));
    }
}

/// Projects a world position to screen pixels with the game camera (AutomataMP `CameraGame::world_to_screen`).
/// `view` rows are the camera's right, up, forward and origin; the camera looks down -forward.
pub fn world_to_screen(view: &[[f32; 4]; 4], fov: f32, screen: [f32; 2], pos: [f32; 3]) -> Option<[f32; 2]> {
    let dot = |a: [f32; 4], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let o = view[3];
    let d = [pos[0] - o[0], pos[1] - o[1], pos[2] - o[2]];
    let z = -dot(view[2], d);
    if z <= 0.01 || screen[1] <= 0.0 {
        return None;
    }
    let y_scale = 1.0 / (fov / 2.0).tan();
    let x_scale = y_scale / (screen[0] / screen[1]);
    let x = dot(view[0], d) / z * x_scale;
    let y = dot(view[1], d) / z * y_scale;
    Some([(1.0 + x) * screen[0] * 0.5, (1.0 - y) * screen[1] * 0.5])
}

impl ImguiRenderLoop for Overlay {
    fn initialize(&mut self, ctx: &mut Context, _: &mut dyn RenderContext) {
        ctx.set_ini_filename(None);
        ctx.set_clipboard_backend(clipboard::WinClipboard);
        ctx.style_mut().window_rounding = 4.0;
    }

    fn before_render(&mut self, ctx: &mut Context, _: &mut dyn RenderContext) {
        self.poll_toggle();
        // The game hides the system cursor; draw ImGui's own while the menu is open.
        ctx.io_mut().mouse_draw_cursor = self.visible || self.chat_box.open;
    }

    fn render(&mut self, ui: &mut Ui) {
        let net = state::NET_STATUS.lock().map(|s| s.clone()).unwrap_or_default();
        let info = state::OVERLAY.lock().map(|o| o.clone()).unwrap_or_default();
        self.draw_name_tag(ui, &net, &info);
        self.chat_box.draw(ui, net.connected);
        if !self.visible {
            let line = match net.rtt_ms {
                Some(r) if net.connected => format!("nier-online: {} | {:.0} ms {}", net.state, r, net.path.unwrap_or("")),
                _ if net.connected => format!("nier-online: {}", net.state),
                _ => format!("nier-online: {} ({} for menu)", net.state, self.cfg.overlay_key),
            };
            ui.get_background_draw_list().add_text([8.0, 8.0], [1.0, 1.0, 1.0, 0.8], line);
            return;
        }
        ui.window(format!("nier-online {} (built {})###nier-online", crate::net::MOD_VERSION, crate::net::BUILT))
            .size([460.0, 420.0], Condition::FirstUseEver)
            .position([40.0, 40.0], Condition::FirstUseEver)
            .build(|| {
                if let Some(_bar) = ui.tab_bar("tabs") {
                    if let Some(_t) = ui.tab_item("Connection") {
                        self.connection_tab(ui, &net, &info);
                    }
                    if let Some(_t) = ui.tab_item("Chat") {
                        self.chat_tab(ui, &net);
                    }
                    if let Some(_t) = ui.tab_item("Settings") {
                        self.settings_tab(ui);
                    }
                    if self.cfg.debug_panel
                        && let Some(_t) = ui.tab_item("Debug")
                    {
                        self.debug_tab(ui, &info);
                    }
                    if let Some(_t) = ui.tab_item("Log") {
                        log_tab(ui);
                    }
                }
            });
    }

    fn message_filter(&self, _io: &Io) -> MessageFilter {
        if self.capturing() { MessageFilter::InputAll } else { MessageFilter::empty() }
    }
}

impl Overlay {
    fn draw_name_tag(&self, ui: &Ui, net: &state::NetStatus, info: &state::OverlayInfo) {
        let (Some((view, fov)), Some(head), true) = (info.camera, info.remote_head, net.connected) else { return };
        let size = ui.io().display_size;
        let Some(p) = world_to_screen(&view, fov, size, head) else { return };
        let name = net.peer_name.clone().unwrap_or_else(|| "Player 2".into());
        let text = match info.remote_hp {
            Some(hp) => format!("{name}  {hp} HP"),
            None => name,
        };
        let w = ui.calc_text_size(&text)[0];
        let dl = ui.get_background_draw_list();
        dl.add_rect([p[0] - w / 2.0 - 4.0, p[1] - 2.0], [p[0] + w / 2.0 + 4.0, p[1] + 16.0], [0.0, 0.0, 0.0, 0.5]).filled(true).build();
        dl.add_text([p[0] - w / 2.0, p[1]], [1.0, 1.0, 1.0, 1.0], text);
    }

    fn connection_tab(&mut self, ui: &Ui, net: &state::NetStatus, info: &state::OverlayInfo) {
        ui.text("Status:");
        ui.same_line();
        ui.text_colored(if net.connected { GREEN } else { GREY }, &net.state);
        if let Some(e) = &net.last_error {
            ui.text_colored(RED, "Error:");
            ui.same_line();
            ui.text_wrapped(e);
        }
        ui.separator();
        if net.connected {
            ui.text(format!("Player: {}", net.peer_name.as_deref().unwrap_or("?")));
            ui.text(format!("Their mod version: {}", net.peer_version.as_deref().unwrap_or("?")));
            match net.rtt_ms {
                Some(r) => ui.text(format!(
                    "Ping: {r:.0} ms, loss {:.1}% ({})",
                    net.loss_pct.unwrap_or(0.0),
                    match net.path {
                        Some("relay") => "through a relay server",
                        Some("direct") => "direct connection",
                        _ => "?",
                    }
                )),
                None => ui.text("Ping: measuring..."),
            }
            if let Some(es) = &info.enemy {
                enemy_stats(ui, es);
            }
            ui.separator();
            self.world_status(ui, net, info);
            if ui.button("Disconnect") {
                let _ = Q.net_cmds.push(NetCommand::Disconnect);
            }
            return;
        }
        if let Some(code) = &net.join_code {
            shared_world_line(ui, info);
            ui.text("Send this join code to your friend:");
            ui.text_wrapped(code);
            if ui.button("Copy join code") {
                ui.set_clipboard_text(code);
            }
            ui.same_line();
            if ui.button("Stop hosting") {
                let _ = Q.net_cmds.push(NetCommand::Disconnect);
            }
            return;
        }
        if net.busy {
            ui.text("Working...");
            if ui.button("Cancel") {
                let _ = Q.net_cmds.push(NetCommand::Disconnect);
            }
            return;
        }
        if self.world_picker.is_some() {
            self.world_picker_panel(ui, info);
            return;
        }
        ui.text("Host a game (your world, enemies and story are used):");
        if ui.button("Host") {
            self.world_picker = Some(crate::net::save::save_dir().map(|d| crate::net::save::slot_infos(&d)).unwrap_or_default());
        }
        ui.separator();
        ui.text("Or join a friend's game:");
        ui.input_text("##code", &mut self.join_code).hint("paste the join code").build();
        ui.same_line();
        if ui.button("Paste") {
            if let Some(t) = ui.clipboard_text() {
                self.join_code = t.trim().to_string();
            }
        }
        let code = self.join_code.trim().to_string();
        if ui.button("Join") && !code.is_empty() {
            self.join_host_world(code);
        }
        ui.text_colored(
            GREY,
            "You play in the host's world: their save shows as \"[online]\" in Load Game (the game restarts first if it \
             has already read its saves). While you are joined, your own saves cannot be loaded and are never changed.",
        );
        self.restart_prompt(ui);
        ui.separator();
        ui.text_colored(GREY, "Testing alone:");
        if ui.button("Mirror test") {
            let _ = Q.net_cmds.push(NetCommand::Loopback);
        }
        ui.same_line();
        ui.text_colored(GREY, "(a copy of you follows 2 m to the side)");
    }

    /// After "Host": which save slot to share. The client gets it as soon as it connects, so the host does not
    /// need to be in that world yet.
    fn world_picker_panel(&mut self, ui: &Ui, info: &state::OverlayInfo) {
        let slots = self.world_picker.clone().unwrap_or_default();
        ui.text("Which world do you want to share?");
        if slots.is_empty() {
            ui.text_colored(GREY, "No saves found.");
        }
        let loaded = savefile::loaded_slot().filter(|_| info.local_player);
        for s in &slots {
            let name = if s.name.is_empty() { "(no name)" } else { s.name.as_str() };
            let now = if loaded == Some(s.slot) { ", playing now" } else { "" };
            if ui.button(format!("Slot {}: {name}  (saved {}{now})", s.slot + 1, ago(s.saved))) {
                let _ = Q.net_cmds.push(NetCommand::Host(Some(s.slot)));
                self.world_picker = None;
            }
        }
        if ui.button("New game (shares your first save once you make one)") {
            let _ = Q.net_cmds.push(NetCommand::Host(None));
            self.world_picker = None;
        }
        ui.text_colored(GREY, "The other player gets this save as soon as they join. Load the same slot yourself.");
        if ui.button("Cancel##host") {
            self.world_picker = None;
        }
    }

    /// Joins `code` in the host's world: right away if the game has not read its save list yet (the list then
    /// waits for the host's save), otherwise after a confirmed restart.
    fn join_host_world(&mut self, code: String) {
        if savefile::listed() {
            self.restart_code = Some(code);
        } else {
            savefile::enter_host_world();
            savefile::hold_next_read(true);
            let _ = Q.net_cmds.push(NetCommand::Join(code));
        }
    }

    fn restart_prompt(&mut self, ui: &Ui) {
        let Some(code) = self.restart_code.clone() else { return };
        ui.separator();
        ui.text_colored(RED, "The game restarts to load the host's world. Unsaved progress is lost.");
        if ui.button("Restart now") {
            if let Err(e) = savefile::restart_into_host_world(&code) {
                self.restart_error = Some(e);
            }
        }
        ui.same_line();
        if ui.button("Cancel##restart") {
            self.restart_code = None;
        }
        if let Some(e) = &self.restart_error {
            ui.text_colored(RED, e);
        }
    }

    /// Which world each side plays in.
    fn world_status(&mut self, ui: &Ui, net: &state::NetStatus, info: &state::OverlayInfo) {
        if state::role() == state::Role::Host {
            shared_world_line(ui, info);
            return;
        }
        if state::role() != state::Role::Client || net.peer_name.as_deref() == Some("mirror") {
            return;
        }
        let (ready, slot) = savefile::host_save();
        match savefile::origin() {
            savefile::Origin::Host => ui.text_colored(GREEN, "You are in the host's world."),
            _ if info.local_player => {
                ui.text_colored(RED, "You are playing your own world, not the host's: the two worlds differ.");
                ui.text_wrapped("Your own saves are protected while connected (saving here does not change them).");
                if let Some(code) = net.joined_code.clone()
                    && ui.button("Restart into host's world")
                {
                    self.restart_code = Some(code);
                }
                self.restart_prompt(ui);
            }
            _ => match slot {
                Some(s) if ready => {
                    let label = if self.cfg.online_save_label { " (shown as \"[online] ...\")" } else { "" };
                    ui.text_colored(GREEN, format!("Host's world ready: Load Game -> slot {}{label}.", s + 1));
                    ui.text_colored(GREY, "Your other slots are blocked while connected. If the list shows your own save");
                    ui.text_colored(GREY, "in that slot, it is the old list: loading it still loads the host's world.");
                }
                _ => ui.text("Waiting for the host's save..."),
            },
        }
    }

    fn chat_tab(&mut self, ui: &Ui, net: &state::NetStatus) {
        let lines = state::CHAT.lock().map(|c| c.iter().map(|l| l.text.clone()).collect::<Vec<_>>()).unwrap_or_default();
        ui.child_window("chatlog").size([0.0, -30.0]).border(true).build(|| {
            for l in &lines {
                ui.text_wrapped(l);
            }
            if ui.scroll_y() >= ui.scroll_max_y() - 4.0 {
                ui.set_scroll_here_y_with_ratio(1.0);
            }
        });
        let send = ui.input_text("##chat", &mut self.chat).enter_returns_true(true).build();
        ui.same_line();
        if (send || ui.button("Send")) && net.connected
            && let Some(text) = chat::clean(&std::mem::take(&mut self.chat))
        {
            let _ = Q.net_cmds.push(NetCommand::Chat(text));
        }
    }

    fn settings_tab(&mut self, ui: &Ui) {
        ui.input_text("Player name", &mut self.cfg.player_name).build();
        ui.input_text("Menu key", &mut self.cfg.overlay_key).build();
        ui.checkbox("Enemy sync (host decides)", &mut self.cfg.enemy_sync);
        ui.checkbox("Replay all enemy animations (client; turn off if enemy fights crash)", &mut self.cfg.enemy_replay_unverified);
        ui.checkbox("Enemies target the nearer player (host)", &mut self.cfg.enemy_target_client);
        ui.checkbox("Die when your partner dies (next session)", &mut self.cfg.shared_death);
        ui.checkbox("Join your partner's cutscenes (next session)", &mut self.cfg.cutscene_sync);
        ui.checkbox("Label the host's save \"[online]\" (client)", &mut self.cfg.online_save_label);
        ui.input_text("Relay URL", &mut self.cfg.relay_url).hint("empty = default public relays").build();
        ui.checkbox("Debug panel", &mut self.cfg.debug_panel);
        ui.input_text("Log level", &mut self.cfg.log_level).build();
        if ui.button("Save") {
            self.cfg.player_name.truncate(crate::net::protocol::MAX_NAME);
            self.save_msg = Some(match config::vk_from_name(&self.cfg.overlay_key) {
                None => ("Unknown key name (try Insert, F8, Home...)".into(), false),
                Some(vk) => match config::set(self.cfg.clone()) {
                    Ok(()) => {
                        self.vk = vk;
                        state::DEBUG_LIST_WANTED.store(self.cfg.debug_panel, Relaxed);
                        crate::sync::aggro::set_enabled(self.cfg.enemy_target_client);
                        ("Saved. Relay and log level apply after a restart.".into(), true)
                    }
                    Err(e) => (format!("Could not save: {e}"), false),
                },
            });
        }
        if let Some((m, ok)) = &self.save_msg {
            ui.text_colored(if *ok { GREEN } else { RED }, m);
        }
    }

    fn debug_tab(&mut self, ui: &Ui, info: &state::OverlayInfo) {
        if !info.game_ok {
            ui.text("Waiting for the game...");
            return;
        }
        ui.input_text("Filter", &mut self.entity_filter).build();
        let filter = self.entity_filter.to_lowercase();
        let selected = state::DEBUG_INSPECT.load(Relaxed);
        ui.child_window("entities").size([0.0, 160.0]).border(true).build(|| {
            for e in info.entities.iter().filter(|e| filter.is_empty() || e.name.to_lowercase().contains(&filter) || e.class.to_lowercase().contains(&filter)) {
                let label = format!(
                    "{:08x} {:<16} {:<14} hp {:<6} ({:.1}, {:.1}, {:.1}){}##{:x}",
                    e.handle,
                    e.name,
                    e.class,
                    e.hp,
                    e.pos[0],
                    e.pos[1],
                    e.pos[2],
                    e.net.map(|(n, k)| format!(" net {n} {k}")).unwrap_or_default(),
                    e.handle
                );
                if ui.selectable_config(&label).selected(selected == e.handle).build() {
                    state::DEBUG_INSPECT.store(if selected == e.handle { 0 } else { e.handle }, Relaxed);
                }
            }
        });
        if let Some(i) = &info.inspect {
            ui.text(format!("{:08x}: {}", i.handle, i.class_chain.join(" : ")));
            ui.child_window("fields").border(true).build(|| {
                for (off, u, f) in &i.rows {
                    ui.text(format!("+{off:05x}  {u:08x}  {u:>11}  {f:.4}"));
                }
            });
        }
    }
}

fn enemy_stats(ui: &Ui, es: &state::EnemyStats) {
    ui.separator();
    if es.host {
        ui.text(format!("Enemy sync (host): sending {} enemies, {} targeting the other player", es.tracked, es.targeting_client));
        return;
    }
    ui.text(format!(
        "Enemy sync: {} host enemies ({} ours, {} spawned copies, {} waiting)",
        es.tracked, es.paired, es.copies, es.waiting
    ));
    ui.text_colored(
        GREY,
        format!(
            "removed {}, killed {}, animations {} replayed / {} refused{}",
            es.removed,
            es.kills,
            es.replayed,
            es.refused,
            if es.replay_unverified { " (unverified on)" } else { "" }
        ),
    );
    if es.host_stale {
        ui.text_colored([1.0, 0.8, 0.3, 1.0], "No enemy updates from the host (loading?)");
    } else if es.no_updates > 0 {
        ui.text_colored([1.0, 0.8, 0.3, 1.0], format!("{} enemies get no updates from the host", es.no_updates));
    }
}

fn log_tab(ui: &Ui) {
    let lines = crate::log::RING.lock().map(|r| r.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
    ui.child_window("log").border(true).horizontal_scrollbar(true).build(|| {
        for l in &lines {
            let color = if l.contains("ERROR") {
                RED
            } else if l.contains("WARN") {
                [1.0, 0.8, 0.3, 1.0]
            } else {
                [0.9, 0.9, 0.9, 1.0]
            };
            ui.text_colored(color, l);
        }
        if ui.scroll_y() >= ui.scroll_max_y() - 4.0 {
            ui.set_scroll_here_y_with_ratio(1.0);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::world_to_screen;

    #[test]
    fn projects_center_and_behind() {
        // Camera at the origin looking down -Z (forward row = +Z), 90 degree fov.
        let view = [[1.0, 0.0, 0.0, 0.0], [0.0, 1.0, 0.0, 0.0], [0.0, 0.0, 1.0, 0.0], [0.0, 0.0, 0.0, 1.0]];
        let fov = std::f32::consts::FRAC_PI_2;
        let p = world_to_screen(&view, fov, [1920.0, 1080.0], [0.0, 0.0, -10.0]).unwrap();
        assert!((p[0] - 960.0).abs() < 0.01 && (p[1] - 540.0).abs() < 0.01);
        // Above the centre line projects higher on screen (smaller y).
        let up = world_to_screen(&view, fov, [1920.0, 1080.0], [0.0, 1.0, -10.0]).unwrap();
        assert!(up[1] < 540.0);
        assert!(world_to_screen(&view, fov, [1920.0, 1080.0], [0.0, 0.0, 10.0]).is_none());
    }
}
