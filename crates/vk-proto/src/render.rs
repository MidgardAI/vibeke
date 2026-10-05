//! Render stream (07 §3): server → client damage frames (state sync, not byte relay),
//! client → server logical input. Opened with `render.attach` on a fresh connection; after the
//! JSON-RPC response the connection switches to `frame` encoding of these enums.

use crate::input::{KeyEvent, MouseEvent};
use crate::model::{ClientFocus, SessionModel};
use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Color {
    #[default]
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

pub mod attr {
    pub const BOLD: u16 = 1;
    pub const DIM: u16 = 1 << 1;
    pub const ITALIC: u16 = 1 << 2;
    pub const UNDERLINE: u16 = 1 << 3;
    pub const DOUBLE_UNDERLINE: u16 = 1 << 4;
    pub const UNDERCURL: u16 = 1 << 5;
    pub const DOTTED_UNDERLINE: u16 = 1 << 6;
    pub const DASHED_UNDERLINE: u16 = 1 << 7;
    pub const INVERSE: u16 = 1 << 8;
    pub const HIDDEN: u16 = 1 << 9;
    pub const STRIKE: u16 = 1 << 10;
    pub const BLINK: u16 = 1 << 11;
    pub const ANY_UNDERLINE: u16 =
        UNDERLINE | DOUBLE_UNDERLINE | UNDERCURL | DOTTED_UNDERLINE | DASHED_UNDERLINE;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Style {
    pub fg: Color,
    pub bg: Color,
    pub ul: Color,
    pub attrs: u16,
}

/// A run of cells sharing one style. `text` holds one grapheme per cell; the spacer cell of a
/// wide character is omitted (the client advances by the shared width function).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Span {
    pub style: Style,
    pub text: String,
    /// Number of terminal columns this span covers.
    pub cols: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Row {
    pub spans: Vec<Span>,
    /// The line continues on the next row (soft wrap); copy mode unwraps.
    pub wrapped: bool,
}

impl Row {
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum CursorShape {
    #[default]
    Block,
    Underline,
    Bar,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub visible: bool,
    pub shape: CursorShape,
    pub blink: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct PaneModes {
    pub alt_screen: bool,
    pub mouse: bool,
    pub bracketed_paste: bool,
    pub focus_events: bool,
    pub app_cursor: bool,
    pub kitty_flags: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DiffOp {
    /// Shift the whole screen up by `n` rows (rows scrolled into history); exposed rows follow.
    ScrollUp {
        n: u16,
    },
    Rows(Vec<(u16, Row)>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AckStatus {
    Written,
    Rejected,
    DroppedOffline,
    Unconfirmed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipSel {
    Clipboard,
    Primary,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Viewport {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PaneRect {
    pub pane: String,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ServerFrame {
    Hello {
        protocol: u32,
        server_version: String,
        session: String,
        machine: String,
        client_id: String,
    },
    /// Full UI model; sent on attach and whenever it changes (coalesced).
    Model {
        model: Box<SessionModel>,
        focus: ClientFocus,
        seen: Vec<(String, u64)>,
    },
    PaneFull {
        pane: String,
        epoch: u32,
        rev: u64,
        cols: u16,
        rows: u16,
        lines: Vec<Row>,
        cursor: Cursor,
        modes: PaneModes,
        title: String,
    },
    PaneDiff {
        pane: String,
        epoch: u32,
        base_rev: u64,
        rev: u64,
        ops: Vec<DiffOp>,
        cursor: Cursor,
        modes: PaneModes,
        title: String,
    },
    /// Reply to `FetchHistory`: scrollback rows, oldest first. `total` = rows available.
    History {
        pane: String,
        req: u64,
        start: u32,
        total: u32,
        lines: Vec<Row>,
    },
    Notify {
        title: String,
        body: String,
        pane: Option<String>,
        urgency: String,
    },
    Bell {
        pane: String,
    },
    Clipboard {
        selection: ClipSel,
        data: Vec<u8>,
        pane: String,
    },
    InputAck {
        input_id: u64,
        status: AckStatus,
    },
    CommandResult {
        req: u64,
        json: String,
    },
    Pong {
        nonce: u64,
        server_ts_ms: i64,
    },
    Goodbye {
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ClientFrame {
    Ack {
        pane: String,
        epoch: u32,
        rev: u64,
    },
    Key {
        input_id: u64,
        pane: String,
        key: KeyEvent,
    },
    RawInput {
        input_id: u64,
        pane: String,
        bytes: Vec<u8>,
    },
    Mouse {
        input_id: u64,
        pane: String,
        event: MouseEvent,
    },
    Paste {
        input_id: u64,
        pane: String,
        text: String,
    },
    Focus {
        pane: String,
    },
    /// Panes visible in this client and their sizes; drives PTY sizing while this client
    /// holds the geometry lease (01 §1.4).
    ViewHint {
        panes: Vec<PaneRect>,
        active: bool,
    },
    Resync {
        pane: String,
    },
    FetchHistory {
        req: u64,
        pane: String,
        start: u32,
        count: u32,
    },
    /// JSON-RPC request piggybacked on the render stream to keep ordering with input.
    Command {
        req: u64,
        json: String,
    },
    Ping {
        nonce: u64,
    },
    Detach,
}
