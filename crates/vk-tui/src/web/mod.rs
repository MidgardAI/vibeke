//! The existing TUI driven by browser events, with no native runtime or PTY.
use crate::app::{App, Machine};
use crate::event::Event;
use crate::screen::{Grid, HostCaps};
use crate::time::Instant;
use vk_proto::frame::FrameBuf;
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, NamedKey};
use vk_proto::render::{ClientFrame, ServerFrame};
use wasm_bindgen::prelude::*;

/// Version of the JavaScript-facing browser interface, independent of the render wire protocol.
#[wasm_bindgen]
pub fn browser_api() -> u32 {
    2
}

#[wasm_bindgen]
pub fn render_protocol() -> u32 {
    vk_proto::render::PROTOCOL
}

#[wasm_bindgen]
pub struct BrowserTui {
    app: App,
    outbox: crate::frame_queue::Sender,
    frames: FrameBuf,
    decoder: crate::input::Decoder,
    last_bytes: Option<Instant>,
    target: Option<(String, String)>,
    hidden: bool,
}

#[wasm_bindgen]
impl BrowserTui {
    #[wasm_bindgen(constructor)]
    pub fn new(label: &str, host: &str) -> Result<BrowserTui, JsValue> {
        let caps = HostCaps {
            truecolor: true,
            undercurl: true,
            osc8: true,
            focus_events: true,
            host_remote: true,
            iterm2_images: true,
            cell_w: 9,
            cell_h: 18,
            dpr_x100: 100,
            ..Default::default()
        };
        let mut app = App::new(
            vk_config::Config::default(),
            vec![Machine::new(label, false)],
            "browser".into(),
            caps,
            true,
            true,
        );
        app.clipboard_sink = Some(Vec::new());
        app.pending_ops = crate::pending::PendingStore::open_browser(host, label)
            .map_err(|e| JsValue::from_str(&e))?;
        let outbox = crate::frame_queue::Sender::default();
        Ok(Self {
            app,
            outbox,
            frames: FrameBuf::default(),
            decoder: Default::default(),
            last_bytes: None,
            target: None,
            hidden: false,
        })
    }

    pub fn connected(&mut self, client_id: &str, features: &str) -> Result<(), JsValue> {
        if self.target.is_none() {
            let focus = &self.app.machines[0].focus;
            self.target = Some((
                focus.workspace.clone().unwrap_or_default(),
                focus.pane.clone().unwrap_or_default(),
            ));
        }
        let features = serde_json::from_str(features).map_err(js_error)?;
        self.outbox = crate::frame_queue::Sender::default();
        self.outbox.set_hidden(self.hidden);
        self.frames = FrameBuf::default();
        self.app.client_id = client_id.into();
        self.app.quit = None;
        let m = &mut self.app.machines[0];
        m.tx = Some(self.outbox.clone());
        m.status = "connected".into();
        m.features = features;
        m.panes.clear();
        m.last_hint.clear();
        self.app.on_connected(0);
        self.app.prev = Grid::new(0, 0);
        self.app.dirty = true;
        Ok(())
    }

    pub fn disconnected(&mut self, reason: &str) {
        self.app.machines[0].tx = None;
        self.app.machines[0].status = reason.into();
        self.app.on_disconnected(0);
        self.outbox = crate::frame_queue::Sender::default();
        self.outbox.set_hidden(self.hidden);
        self.frames = FrameBuf::default();
        self.decoder = Default::default();
        self.last_bytes = None;
        self.app.dirty = true;
    }

    pub fn receive(&mut self, bytes: &[u8]) -> Result<(), JsValue> {
        if bytes.len() > 64 * 1024 {
            return Err(JsValue::from_str("Render chunk is too large"));
        }
        self.frames.push(bytes);
        while let Some(f) = self.frames.next_frame::<ServerFrame>().map_err(js_error)? {
            let model = matches!(&f, ServerFrame::Model { .. });
            self.app.on_frame(0, f);
            if model && let Some((workspace, pane)) = self.target.take() {
                if !pane.is_empty()
                    && self.app.machines[0]
                        .model
                        .panes
                        .iter()
                        .any(|p| p.id == pane)
                {
                    self.app.focus_pane(0, &pane);
                } else if !workspace.is_empty() {
                    self.app.command_on(
                        0,
                        "workspace.focus",
                        serde_json::json!({"workspace": workspace}),
                        crate::app::Pending::Ignore,
                    );
                }
            }
            self.app.dirty = true;
        }
        Ok(())
    }

    /// xterm.js supplies text, composed IME input, and mouse sequences here.
    pub fn input(&mut self, bytes: &[u8]) {
        if bytes.len() > 128 * 1024 {
            self.decoder = Default::default();
            self.app
                .toast("Paste is too large (128 KiB maximum); nothing was sent");
            return;
        }
        for ev in self.decoder.feed(bytes) {
            self.app.on_input(ev);
        }
        self.last_bytes = Some(Instant::now());
    }

    pub fn paste(&mut self, text: &str) {
        if text.len() > 128 * 1024 {
            self.app
                .toast("Paste is too large (128 KiB maximum); nothing was sent");
            return;
        }
        self.app.on_event(Event::Paste(text.into()));
    }

    /// Logical DOM keys preserve modified Enter and palette shortcuts that legacy bytes lose.
    pub fn key(&mut self, name: &str, mods: u8, repeat: bool, release: bool) -> bool {
        // Logical key events bypass the byte decoder. Settle any preceding Escape first
        // so fast Escape + Ctrl+B cannot be reordered or turn the next text into Alt+text.
        if !release {
            for event in self.decoder.flush() {
                self.app.on_input(event);
            }
            self.last_bytes = None;
        }
        let named = match name {
            "Enter" => Some(NamedKey::Enter),
            "Tab" => Some(NamedKey::Tab),
            "Backspace" => Some(NamedKey::Backspace),
            "Escape" => Some(NamedKey::Escape),
            "ArrowUp" => Some(NamedKey::Up),
            "ArrowDown" => Some(NamedKey::Down),
            "ArrowLeft" => Some(NamedKey::Left),
            "ArrowRight" => Some(NamedKey::Right),
            "Home" => Some(NamedKey::Home),
            "End" => Some(NamedKey::End),
            "PageUp" => Some(NamedKey::PageUp),
            "PageDown" => Some(NamedKey::PageDown),
            "Insert" => Some(NamedKey::Insert),
            "Delete" => Some(NamedKey::Delete),
            n if n.starts_with('F') => n[1..]
                .parse::<u8>()
                .ok()
                .filter(|n| (1..=24).contains(n))
                .map(NamedKey::F),
            _ => None,
        };
        let key = if let Some(n) = named {
            Key::Named(n)
        } else if name.chars().count() == 1 {
            Key::Char(name.chars().next().unwrap())
        } else {
            return false;
        };
        let mut ev = KeyEvent::new(key, Mods(mods));
        ev.kind = if release {
            KeyKind::Release
        } else if repeat {
            KeyKind::Repeat
        } else {
            KeyKind::Press
        };
        if let Key::Char(c) = key
            && !ev.mods.ctrl()
            && !ev.mods.sup()
        {
            ev.text = Some(c.to_string());
        }
        self.app.on_input(crate::input::Input::Key(ev));
        true
    }

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u16, cell_h: u16, dpr_x100: u16) {
        self.app.caps.dpr_x100 = dpr_x100.clamp(25, 800);
        self.app.caps.cell_w = cell_w.max(1);
        self.app.caps.cell_h = cell_h.max(1);
        crate::term::set_cell_px(cell_w, cell_h);
        self.app
            .on_event(Event::Resize(cols.clamp(2, 500), rows.clamp(1, 300)));
    }
    pub fn focus(&mut self, focused: bool) {
        self.app.on_event(if focused {
            Event::FocusGained
        } else {
            Event::FocusLost
        });
    }
    pub fn action(&mut self, name: &str) {
        self.app.action(name, None);
        self.app.dirty = true;
    }
    pub fn toast(&mut self, message: &str) {
        self.app.toast(message);
    }
    pub fn quit_reason(&self) -> Option<String> {
        self.app.quit.clone()
    }

    pub fn focus_target(&mut self, workspace: &str, pane: &str) {
        self.target = Some((workspace.into(), pane.into()));
    }

    pub fn location(&self) -> String {
        let f = &self.app.machines[0].focus;
        serde_json::json!({"workspace":f.workspace, "pane":f.pane}).to_string()
    }

    /// Protocol deadlines do not depend on requestAnimationFrame or xterm write completion.
    pub fn tick(&mut self) -> Option<u32> {
        let now = Instant::now();
        if let Some(at) = self.last_bytes
            && self
                .decoder
                .wait()
                .is_some_and(|wait| now.duration_since(at) >= wait)
        {
            for ev in self.decoder.flush() {
                self.app.on_input(ev);
            }
            self.last_bytes = None;
        }
        self.app.on_tick();
        if self.app.next_deadline(now).is_some_and(|t| t <= now) {
            self.app.on_deadline(now);
        }
        let deadline = self.app.next_deadline(now);
        let decoder = self
            .last_bytes
            .and_then(|at| self.decoder.wait().map(|wait| at + wait));
        deadline.into_iter().chain(decoder).min().map(|at| {
            at.saturating_duration_since(now)
                .as_millis()
                .clamp(5, 2_147_483_647) as u32
        })
    }

    pub fn dirty(&self) -> bool {
        self.app.dirty
    }

    pub fn render(&mut self) -> Vec<u8> {
        if self.app.dirty {
            self.app.draw_bytes()
        } else {
            Vec::new()
        }
    }

    pub fn outgoing(&mut self) -> Result<Vec<u8>, JsValue> {
        self.outbox.drain().map_err(js_error)
    }

    pub fn appearance(&mut self, light: bool) {
        self.app.config.theme.mode = if light {
            vk_config::ThemeMode::Light
        } else {
            vk_config::ThemeMode::Dark
        };
        crate::appearance::apply(&mut self.app, true);
    }

    /// Stop cell/media subscriptions while hidden. Restore them on the next visible draw.
    pub fn visible(&mut self, visible: bool) {
        self.hidden = !visible;
        self.outbox.set_hidden(self.hidden);
        if !visible {
            self.app.machines[0].send(ClientFrame::ViewHint {
                panes: vec![],
                active: false,
            });
            self.app.machines[0].send(ClientFrame::MediaView {
                panes: vec![],
                shm: false,
                key_releases: false,
            });
        }
        crate::browser::on_connected(&mut self.app, 0);
        self.app.machines[0].last_hint.clear();
        self.app.prev = Grid::new(0, 0);
        self.app.dirty = true;
    }

    pub fn take_url(&mut self) -> Option<String> {
        let last = self.app.nav.opened.pop();
        self.app.nav.opened.clear();
        last
    }

    pub fn take_download(&mut self) -> Option<js_sys::Array> {
        let (name, bytes) = self.app.gallery.downloads.pop()?;
        let result = js_sys::Array::new();
        result.push(&JsValue::from_str(&name));
        result.push(&js_sys::Uint8Array::from(bytes.as_slice()));
        Some(result)
    }

    pub fn take_clipboard(&mut self) -> Option<String> {
        let sink = self.app.clipboard_sink.as_mut()?;
        let last = sink.pop();
        sink.clear();
        last.map(|(b, _)| String::from_utf8_lossy(&b).into_owned())
    }
}
fn js_error(e: impl std::fmt::Display) -> JsValue {
    JsValue::from_str(&e.to_string())
}
