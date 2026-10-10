//! The existing TUI driven by browser events, with no native runtime or PTY.
use crate::app::{App, Machine};
use crate::event::Event;
use crate::screen::{Grid, HostCaps};
use crate::time::Instant;
use tokio::sync::mpsc;
use vk_proto::frame::{self, FrameBuf};
use vk_proto::input::{Key, KeyEvent, KeyKind, Mods, NamedKey};
use vk_proto::render::{ClientFrame, ServerFrame};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
pub fn render_protocol() -> u32 {
    vk_proto::render::PROTOCOL
}

#[wasm_bindgen]
pub struct BrowserTui {
    app: App,
    rx: mpsc::UnboundedReceiver<ClientFrame>,
    frames: FrameBuf,
    decoder: crate::input::Decoder,
    last_bytes: Option<Instant>,
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
        let key = format!("vibeke-tui-pending:{host}");
        if let Some(storage) = web_sys::window().and_then(|w| w.session_storage().ok().flatten())
            && let Ok(Some(saved)) = storage.get_item(&key)
        {
            app.pending_ops.ops = serde_json::from_str(&saved).map_err(|_| {
                JsValue::from_str(
                    "Pending TUI operations could not be read. Keep browser storage for recovery.",
                )
            })?;
        }
        // This storage key belongs to exactly one host, even if its display name changed.
        for op in &mut app.pending_ops.ops {
            op.machine = label.into();
        }
        app.pending_ops.browser_key = Some(key);
        let (_, rx) = mpsc::unbounded_channel();
        Ok(Self {
            app,
            rx,
            frames: FrameBuf::default(),
            decoder: Default::default(),
            last_bytes: None,
        })
    }

    pub fn connected(&mut self, client_id: &str, features: &str) -> Result<(), JsValue> {
        let features = serde_json::from_str(features).map_err(js_error)?;
        let (tx, rx) = mpsc::unbounded_channel();
        self.rx = rx;
        self.frames = FrameBuf::default();
        self.app.client_id = client_id.into();
        self.app.quit = None;
        let m = &mut self.app.machines[0];
        m.tx = Some(tx);
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
        while self.rx.try_recv().is_ok() {}
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
            self.app.on_frame(0, f);
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

    pub fn resize(&mut self, cols: u16, rows: u16, cell_w: u16, cell_h: u16) {
        self.app.caps.cell_w = cell_w.max(1);
        self.app.caps.cell_h = cell_h.max(1);
        crate::term::set_cell_px(cell_w, cell_h);
        self.app
            .on_event(Event::Resize(cols.clamp(20, 500), rows.clamp(5, 300)));
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

    pub fn render(&mut self) -> Vec<u8> {
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
        if self.app.dirty {
            self.app.draw_bytes()
        } else {
            Vec::new()
        }
    }

    pub fn outgoing(&mut self) -> Result<Vec<u8>, JsValue> {
        let mut out = Vec::new();
        // Bound each gateway batch. Remaining frames are drained on the next browser frame.
        for _ in 0..64 {
            let Ok(f) = self.rx.try_recv() else {
                break;
            };
            let bytes = frame::encode(&f).map_err(js_error)?;
            if bytes.len() > 256 * 1024 {
                return Err(JsValue::from_str("TUI input frame is too large"));
            }
            out.extend(bytes);
            if out.len() >= 256 * 1024 {
                break;
            }
        }
        Ok(out)
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
