//! The ImGui overlay, drawn by hudhook from the DX11 swap chain (the approach of AutomataMP
//! `D3D11Hook.cpp` / `WindowsMessageHook.cpp`). It never touches game memory: it reads the status structs
//! in `crate::state` and sends commands through queues.

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
    cfg: Config,
    save_msg: Option<(String, bool)>,
    entity_filter: String,
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
            cfg,
            save_msg: None,
            entity_filter: String::new(),
        }
    }

    /// Edge-detects the toggle key, only while this game window has focus (two copies can run side by side).
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
            state::OVERLAY_VISIBLE.store(self.visible, Relaxed);
            if self.visible {
                cursor::release();
            }
            state::DEBUG_LIST_WANTED.store(self.visible && self.cfg.debug_panel, Relaxed);
        }
        self.key_was_down = down;
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
        ctx.io_mut().mouse_draw_cursor = self.visible;
    }

    fn render(&mut self, ui: &mut Ui) {
        let net = state::NET_STATUS.lock().map(|s| s.clone()).unwrap_or_default();
        let info = state::OVERLAY.lock().map(|o| o.clone()).unwrap_or_default();
        self.draw_name_tag(ui, &net, &info);
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
        if self.visible { MessageFilter::InputAll } else { MessageFilter::empty() }
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
                Some(r) => ui.text(format!("Ping: {r:.0} ms ({})", net.path.unwrap_or("?"))),
                None => ui.text("Ping: measuring..."),
            }
            if let Some(es) = &info.enemy {
                enemy_stats(ui, es);
            }
            if ui.button("Disconnect") {
                let _ = Q.net_cmds.push(NetCommand::Disconnect);
            }
            return;
        }
        if let Some(code) = &net.join_code {
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
        ui.text("Host a game (your world, enemies and story are used):");
        if ui.button("Host") {
            let _ = Q.net_cmds.push(NetCommand::Host);
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
        if ui.button("Join") && !self.join_code.trim().is_empty() {
            let _ = Q.net_cmds.push(NetCommand::Join(self.join_code.trim().to_string()));
        }
        ui.separator();
        ui.text_colored(GREY, "Testing alone:");
        if ui.button("Mirror test") {
            let _ = Q.net_cmds.push(NetCommand::Loopback);
        }
        ui.same_line();
        ui.text_colored(GREY, "(a copy of you follows 2 m to the side)");
    }

    fn chat_tab(&mut self, ui: &Ui, net: &state::NetStatus) {
        let lines = state::CHAT.lock().map(|c| c.iter().cloned().collect::<Vec<_>>()).unwrap_or_default();
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
        if (send || ui.button("Send")) && net.connected && !self.chat.trim().is_empty() {
            let _ = Q.net_cmds.push(NetCommand::Chat(std::mem::take(&mut self.chat)));
        }
    }

    fn settings_tab(&mut self, ui: &Ui) {
        ui.input_text("Player name", &mut self.cfg.player_name).build();
        ui.input_text("Menu key", &mut self.cfg.overlay_key).build();
        ui.checkbox("Enemy sync (host decides)", &mut self.cfg.enemy_sync);
        ui.checkbox("Replay all enemy animations (client; turn off if enemy fights crash)", &mut self.cfg.enemy_replay_unverified);
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
        ui.text(format!("Enemy sync (host): sending {} enemies", es.tracked));
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
