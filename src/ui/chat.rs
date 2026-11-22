//! The chat box in the bottom-left of the screen. `/` (whichever key types '/' on this keyboard layout)
//! opens it, and while it is open it takes the keyboard the way the menu does. Enter sends, Escape closes,
//! and `/` on an empty line closes it again. While it is closed, new lines show there for a few seconds and
//! then fade out.

use hudhook::imgui::{Condition, Key, StyleColor, Ui};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL, VK_ESCAPE, VK_MENU, VK_RETURN, VK_SHIFT, VkKeyScanW};

use crate::net::protocol::MAX_CHAT;
use crate::state::{self, ChatLine, NetCommand, Q};

/// How long a new line stays in the closed box, and how long its fade-out at the end lasts.
const SHOW_MS: f64 = 8000.0;
const FADE_MS: f64 = 1500.0;
/// Lines shown while the box is closed.
const FEED_LINES: usize = 8;
/// A '/' that reaches the input this soon after opening is the key press that opened the box.
const OPEN_CHAR_MS: f64 = 250.0;

fn key_down(vk: u16) -> bool {
    // SAFETY: plain Win32 query.
    unsafe { GetAsyncKeyState(vk as i32) as u16 & 0x8000 != 0 }
}

/// Chat text as the peer's decoder accepts it: no control characters, at most `MAX_CHAT` bytes (cut on a
/// character boundary). `None` when nothing is left to send.
pub fn clean(text: &str) -> Option<String> {
    let mut s: String = text.chars().filter(|c| !c.is_control()).collect();
    let mut end = s.trim_end().len().min(MAX_CHAT);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    let s = s.trim_start().to_string();
    (!s.is_empty()).then_some(s)
}

pub struct ChatBox {
    pub open: bool,
    input: String,
    opened_ms: f64,
    focus: bool,
    key_was_down: bool,
    /// After the box closes, game input stays blocked until Enter and Escape are released, so the game
    /// doesn't see the key that closed it (Escape would open the pause menu).
    hold: bool,
    /// The key that types '/', and the modifiers it needs (Shift, Ctrl, Alt) on this layout.
    vk: u16,
    mods: u8,
}

impl ChatBox {
    pub fn new() -> Self {
        // SAFETY: plain Win32 query. Low byte: virtual key; high byte: modifiers; -1: no key types '/'.
        let r = unsafe { VkKeyScanW('/' as u16) };
        let (vk, mods) = if r == -1 { (0xBF, 0) } else { ((r & 0xFF) as u16, ((r >> 8) & 0x7) as u8) };
        Self { open: false, input: String::new(), opened_ms: 0.0, focus: false, key_was_down: false, hold: false, vk, mods }
    }

    /// Game input is blocked: the box is open, or a key that closed it is still held.
    pub fn captures(&self) -> bool {
        self.open || self.hold
    }

    /// Opens the box on `/`. `focused`: this game window has focus; `menu_open`: the menu has the keyboard.
    pub fn poll(&mut self, focused: bool, menu_open: bool) {
        if self.hold && !key_down(VK_RETURN.0) && !key_down(VK_ESCAPE.0) {
            self.hold = false;
        }
        let mods_ok = [(1, VK_SHIFT), (2, VK_CONTROL), (4, VK_MENU)].iter().all(|&(bit, vk)| self.mods & bit == 0 || key_down(vk.0));
        let down = focused && key_down(self.vk) && mods_ok;
        if down && !self.key_was_down && !self.open && !menu_open {
            self.open = true;
            self.focus = true;
            self.opened_ms = state::now_ms();
            self.input.clear();
        }
        self.key_was_down = down;
    }

    fn close(&mut self) {
        self.open = false;
        self.hold = true;
        self.input.clear();
    }

    pub fn draw(&mut self, ui: &Ui, connected: bool) {
        let [w, h] = ui.io().display_size;
        let width = (w * 0.35).clamp(280.0, 560.0);
        // Bottom-left corner of the box.
        let pos = [16.0, h - 96.0];
        let lines: Vec<ChatLine> = state::CHAT.lock().map(|c| c.iter().cloned().collect()).unwrap_or_default();
        if self.open {
            self.draw_open(ui, connected, &lines, pos, [width, (h * 0.3).clamp(160.0, 380.0)]);
        } else {
            draw_feed(ui, &lines, pos, width);
        }
    }

    fn draw_open(&mut self, ui: &Ui, connected: bool, lines: &[ChatLine], pos: [f32; 2], size: [f32; 2]) {
        let mut enter = false;
        ui.window("##nier-online-chat")
            .position(pos, Condition::Always)
            .position_pivot([0.0, 1.0])
            .size(size, Condition::Always)
            .title_bar(false)
            .resizable(false)
            .movable(false)
            .collapsible(false)
            .bg_alpha(0.75)
            .build(|| {
                ui.child_window("##chatlog").size([0.0, -ui.frame_height_with_spacing()]).build(|| {
                    for l in lines {
                        ui.text_wrapped(&l.text);
                    }
                    if ui.scroll_y() >= ui.scroll_max_y() - 4.0 {
                        ui.set_scroll_here_y_with_ratio(1.0);
                    }
                });
                if self.focus {
                    ui.set_keyboard_focus_here();
                    self.focus = false;
                }
                let _w = ui.push_item_width(-1.0);
                let hint = if connected { "Enter: send, Esc: close" } else { "not connected (Esc: close)" };
                enter = ui.input_text("##chatinput", &mut self.input).hint(hint).enter_returns_true(true).build();
            });
        if self.input == "/" {
            if state::now_ms() - self.opened_ms < OPEN_CHAR_MS {
                self.input.clear(); // the key press that opened the box
            } else {
                self.close(); // `/` on an empty line
            }
            return;
        }
        if ui.is_key_pressed(Key::Escape) {
            self.close();
            return;
        }
        if enter {
            match clean(&self.input) {
                Some(text) if connected => {
                    let _ = Q.net_cmds.push(NetCommand::Chat(text));
                }
                Some(_) => state::push_chat("(not connected: message not sent)".into()),
                None => {}
            }
            self.close();
        }
    }
}

/// The closed box: the newest lines, each fading out `SHOW_MS` after it arrived. Click-through.
fn draw_feed(ui: &Ui, lines: &[ChatLine], pos: [f32; 2], width: f32) {
    let now = state::now_ms();
    let mut recent: Vec<(&ChatLine, f32)> = lines
        .iter()
        .rev()
        .take(FEED_LINES)
        .filter_map(|l| {
            let left = SHOW_MS - (now - l.at_ms);
            (left > 0.0).then(|| (l, (left / FADE_MS).min(1.0) as f32))
        })
        .collect();
    if recent.is_empty() {
        return;
    }
    recent.reverse();
    let top = recent.iter().map(|r| r.1).fold(0.0, f32::max);
    ui.window("##nier-online-chatfeed")
        .position(pos, Condition::Always)
        .position_pivot([0.0, 1.0])
        .size([width, 0.0], Condition::Always) // 0: fit the height to the lines
        .no_decoration()
        .no_inputs()
        .no_nav()
        .focus_on_appearing(false)
        .bg_alpha(0.4 * top)
        .build(|| {
            for (l, a) in recent {
                let _c = ui.push_style_color(StyleColor::Text, [1.0, 1.0, 1.0, a]);
                ui.text_wrapped(&l.text);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_makes_text_the_decoder_accepts() {
        assert_eq!(clean("  hi there \t"), Some("hi there".into()));
        assert_eq!(clean("a\nb"), Some("ab".into()));
        assert_eq!(clean("   "), None);
        let long = "é".repeat(200); // 400 bytes
        let c = clean(&long).unwrap();
        assert!(c.len() <= MAX_CHAT && c.chars().all(|ch| ch == 'é'));
    }
}
