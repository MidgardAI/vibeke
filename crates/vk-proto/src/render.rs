//! Render stream (07 §3): server → client damage frames (state sync, not byte relay),
//! client → server logical input. Opened with `render.attach` on a fresh connection; after the
//! JSON-RPC response the connection switches to `frame` encoding of these enums.

use crate::input::{KeyEvent, MouseEvent};
use crate::model::{ClientFocus, SessionModel};
use serde::{Deserialize, Serialize};

/// Render protocol version. Frames and the model are postcard-encoded, and postcard is
/// positional: a new struct field (even `#[serde(default)]`, even `None`) shifts everything
/// after it, so any change to a type reachable from [`ServerFrame`]/[`ClientFrame`] bumps
/// this, and `render.attach` refuses a client whose version differs (`version_mismatch`).
///
/// - 1: before Goal 03 (no browser panes).
/// - 2: `Pane.browser`, media frames (`ServerFrame::Media`/`BrowserState`, sent in parts that
///   share a `seq`, only the first with `reset`), `ClientFrame::MediaView`/`MediaAck`/`Browser`.
/// - 3: `BrowserPane.device`/`viewport` (pinned, letterboxed viewports) and
///   `BrowserCmd::DropFiles` (files into the page, 06 B3.2).
/// - 4: terminal effects (03 §8): `Row.mark` (OSC 133 prompt rows) and `Row.links` (OSC 8),
///   `SessionModel.pane_live` (OSC 9;4 progress, last exit code, user vars),
///   `ServerFrame::ClipboardQuery` / `ClientFrame::ClipboardReply` (OSC 52 read), and
///   `ServerFrame::Image` / `PaneImages` (inbound kitty graphics, 03 §9).
pub const PROTOCOL: u32 = 4;

/// `render.attach` error kind when client and server speak different render protocols.
pub const VERSION_MISMATCH: &str = "version_mismatch";

/// Client side of the negotiation: the `render.attach` reply must carry our [`PROTOCOL`]
/// (an older server answers with its own number, or without one). Returns an error message
/// with an upgrade hint otherwise.
pub fn check_attach_reply(reply: &serde_json::Value) -> Result<(), String> {
    if let Some(e) = reply.get("error") {
        return Err(format!(
            "render.attach: {}",
            e.get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("refused")
        ));
    }
    let theirs = reply
        .get("result")
        .and_then(|r| r.get("protocol"))
        .and_then(|p| p.as_u64())
        .unwrap_or(1);
    if theirs != PROTOCOL as u64 {
        return Err(format!(
            "{VERSION_MISMATCH}: the server speaks render protocol {theirs}, this client {PROTOCOL}; upgrade the older side (`vibeke machine upgrade <machine>` for a remote, or restart the server from this build)"
        ));
    }
    Ok(())
}

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
    /// OSC 133 shell-integration state of the row ([`mark`]): copy-mode prompt jumps and
    /// "select command output" (03 §8). Appended (render protocol 4).
    #[serde(default)]
    pub mark: u8,
    /// OSC 8 hyperlinks on this row, left to right, non-overlapping (03 §8). Appended (render
    /// protocol 4).
    #[serde(default)]
    pub links: Vec<Link>,
}

/// Values of [`Row::mark`].
pub mod mark {
    /// Not a prompt row (command output, or no shell integration).
    pub const NONE: u8 = 0;
    /// The row where a prompt starts (`OSC 133 ; A`).
    pub const PROMPT: u8 = 1;
    /// A continuation row of a multi-line prompt or command line.
    pub const PROMPT_CONT: u8 = 2;
}

/// An OSC 8 hyperlink run: `cols` cells from column `col` link to `uri`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Link {
    pub col: u16,
    pub cols: u16,
    pub uri: String,
}

impl Row {
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// A row of plain spans (no prompt mark, no links).
    pub fn new(spans: Vec<Span>, wrapped: bool) -> Self {
        Row {
            spans,
            wrapped,
            ..Default::default()
        }
    }

    /// The hyperlink covering column `col`, if any.
    pub fn link_at(&self, col: u16) -> Option<&Link> {
        self.links
            .iter()
            .find(|l| col >= l.col && col < l.col.saturating_add(l.cols))
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
        /// Channels the server already delivered to (`native`): a client then skips its own
        /// host-terminal OSC forward (08 §7.1). Appended (postcard).
        #[serde(default)]
        delivered: Vec<String>,
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
    /// Media channel (03 §5, 06 B3.2): changed tiles of a browser pane, latest-wins per pane,
    /// sent after cell frames and only for panes this client reported visible
    /// (`ClientFrame::MediaView`). Acked with `ClientFrame::MediaAck`.
    Media(Box<MediaFrame>),
    /// Browser pane chrome state (URL, loading, history buttons, environment label).
    BrowserState {
        pane: String,
        state: BrowserStatus,
    },
    /// Event push (07 §3): events matching the client's `ClientFrame::Subscribe` filters, in
    /// sequence order. `lagged` = the server dropped events for this client (its buffer
    /// overflowed); the client catches up with `events.read` from its cursor.
    Events {
        events: Vec<PushedEvent>,
        lagged: bool,
    },
    /// OSC 52 read (03 §8): a pane asked for the clipboard. Sent to one client (the most
    /// recently active one showing the pane); the client applies `clipboard.osc52_read`
    /// (deny / ask / allow) and answers with `ClientFrame::ClipboardReply`. Appended
    /// (render protocol 4).
    ClipboardQuery {
        req: u64,
        pane: String,
        selection: ClipSel,
    },
    /// Inbound kitty graphics (03 §9): an image's pixels, sent once per client per content
    /// hash before the first `PaneImages` that places it. `rgba_z` is zlib-deflated RGBA of
    /// `width × height`. Appended (render protocol 4).
    Image {
        hash: String,
        width: u32,
        height: u32,
        rgba_z: Vec<u8>,
    },
    /// The visible kitty placements of a terminal pane (replaces the previous set; empty =
    /// none). Sent after the cell frame whenever the set or a position changed. Appended
    /// (render protocol 4).
    PaneImages {
        pane: String,
        epoch: u32,
        places: Vec<ImagePlace>,
    },
}

/// One kitty image placement on a terminal pane (03 §9): image `hash` scaled into `cols ×
/// rows` cells whose top-left is (`col`, `row`) of the pane screen (negative when partly
/// scrolled off; the client clips).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ImagePlace {
    pub hash: String,
    pub width: u32,
    pub height: u32,
    pub col: i32,
    pub row: i32,
    pub cols: u16,
    pub rows: u16,
    pub z: i32,
}

/// One pushed event: its sequence number and type for cheap routing, and the full event as
/// JSON (the same object `events.read` returns: `seq, ts, type, subject, actor, data`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushedEvent {
    pub seq: i64,
    pub kind: String,
    pub json: String,
}

/// Changed tiles of one browser pane frame. Tiles are cell-aligned: tile `index` covers
/// `tile_cols × tile_rows` cells starting at cell (`col`, `row`) of the pane's content area;
/// edge tiles may be smaller. Tiles not listed are unchanged since the last frame this client
/// received for the pane, except after `reset` (geometry changed: every tile is included).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaFrame {
    pub pane: String,
    pub seq: u64,
    /// Frame size in device pixels.
    pub width: u32,
    pub height: u32,
    /// Host cell size (device px) the tile grid was cut for.
    pub cell_w: u16,
    pub cell_h: u16,
    pub tile_cols: u16,
    pub tile_rows: u16,
    /// Grid size in tiles.
    pub grid_cols: u16,
    pub grid_rows: u16,
    pub reset: bool,
    pub tiles: Vec<MediaTile>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaTile {
    pub index: u32,
    /// Top-left cell of the tile inside the pane content area.
    pub col: u16,
    pub row: u16,
    /// Cells covered (edge tiles may be narrower/shorter).
    pub cols: u16,
    pub rows: u16,
    /// Pixel size.
    pub w: u32,
    pub h: u32,
    pub data: TileData,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TileData {
    /// POSIX shared memory object holding `len` bytes of RGBA (same machine only). The host
    /// terminal unlinks it after reading (kitty `t=s`); a client that drops the tile unlinks it.
    Shm { name: String, len: u32 },
    /// zlib-compressed RGBA (kitty `o=z`).
    ZlibRgba(Vec<u8>),
    /// Raw RGBA.
    Rgba(Vec<u8>),
}

/// What the browser pane's one-row chrome shows (06 B3.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct BrowserStatus {
    pub url: String,
    pub title: String,
    pub loading: bool,
    pub can_back: bool,
    pub can_forward: bool,
    /// `laptop chromium → devbox loopback`.
    pub env: String,
    /// The profile is open in a headful window (06 B3.3); the pane shows no frames.
    pub windowed: bool,
    pub error: Option<String>,
    /// One-shot message for a toast (screenshot saved, …).
    pub notice: Option<String>,
    /// CSS viewport size.
    pub css_w: u32,
    pub css_h: u32,
    /// Watch mode (06 B7): the agent browser session shown (`b3`); `None` for an ordinary
    /// browser pane.
    #[serde(default)]
    pub watch: Option<String>,
    /// A human has taken the watched session over (input goes to the page, the agent gets
    /// `human_control` errors).
    #[serde(default)]
    pub human_control: bool,
    /// The take-over is held by this pane (`prefix+t` releases it; closing the pane does too).
    #[serde(default)]
    pub controlled_here: bool,
}

/// A browser pane visible on the client, with the geometry the viewport follows (06 B3.2
/// "Crisp sizing"): content cells × host cell px, at `dpr`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MediaPane {
    pub pane: String,
    /// Label of the machine that owns the pane (its layout and persisted state) when that is
    /// not the server receiving this; empty = this server. A remote owner is also the route
    /// target: `localhost` means that machine.
    pub owner: String,
    /// The pane's persisted browser state as the owner reported it.
    pub spec: crate::model::BrowserPane,
    /// Content area in cells (pane rect minus the chrome row).
    pub cols: u16,
    pub rows: u16,
    /// Host cell size in device pixels and the device pixel ratio.
    pub cell_w: u16,
    pub cell_h: u16,
    pub dpr: f32,
}

/// Input and commands for a browser pane, sent to the server that renders it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum BrowserCmd {
    Key(KeyEvent),
    /// Paste / IME commit → `Input.insertText`.
    Text(String),
    /// Mouse in CSS px of the viewport.
    Mouse {
        kind: crate::input::MouseKind,
        button: crate::input::MouseButton,
        x: f32,
        y: f32,
        mods: crate::input::Mods,
        clicks: u8,
    },
    /// Wheel with pixel deltas (CSS px).
    Wheel {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        mods: crate::input::Mods,
    },
    Navigate(String),
    Back,
    Forward,
    Reload {
        hard: bool,
    },
    Stop,
    /// Hand the profile to a headful window (true) or back to the pane (false) (06 B3.3).
    Window(bool),
    Screenshot,
    /// Watch mode (06 B7): take the watched agent session over (true) or release it (false).
    TakeOver(bool),
    /// Files the user confirmed for the page (a dropped/pasted path, a clipboard image saved
    /// to the server's inbox): paths on the rendering server's machine, each a readable
    /// regular file ≤ 50 MiB. They go to an open file chooser (`DOM.setFileInputFiles`), else
    /// are dropped on the page at the last pointer position (`Input.dispatchDragEvent`).
    DropFiles(Vec<String>),
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
    /// Browser panes visible in this client (replaces the previous set). Panes not listed get
    /// no media frames; a pane no client shows stops its screencast.
    MediaView {
        panes: Vec<MediaPane>,
        /// The client can hand shared-memory tiles to its host (same machine as this server and
        /// the host supports kitty `t=s`).
        shm: bool,
        /// The host reports key releases (kitty keyboard event types).
        key_releases: bool,
    },
    MediaAck {
        pane: String,
        seq: u64,
    },
    Browser {
        input_id: u64,
        pane: String,
        cmd: BrowserCmd,
    },
    /// Event push (07 §3): replace this client's event subscription. `types` are globs as in
    /// `events.read` (`client.confirm_*`, `interaction.*`); empty = unsubscribe. With `after`,
    /// matching events after that sequence number are replayed first. Only sent to servers
    /// whose `render.attach` result lists the `event_push` feature (older servers would drop
    /// the connection on an unknown frame).
    Subscribe {
        types: Vec<String>,
        after: Option<i64>,
    },
    /// The client's viewport of `pane` moved in its scrollback (copy mode): `offset` rows above
    /// the live screen (0 = back at the bottom) of `total` scrollback rows the client knows.
    /// Feeds Herdr's `pane.scroll_changed` (07 §8.3). Only sent to servers whose
    /// `render.attach` result lists the `scroll_report` feature. Appended (postcard).
    ScrollView {
        pane: String,
        offset: u32,
        total: u32,
    },
    /// Answer to `ServerFrame::ClipboardQuery`: the clipboard text, or `None` when the client
    /// denied the read (policy or user). Appended (render protocol 4).
    ClipboardReply {
        req: u64,
        pane: String,
        data: Option<Vec<u8>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{decode, encode};
    use crate::model::BrowserPane;

    fn rt_server(f: &ServerFrame) -> ServerFrame {
        decode(&encode(f).unwrap()[4..]).unwrap()
    }

    fn rt_client(f: &ClientFrame) -> ClientFrame {
        decode(&encode(f).unwrap()[4..]).unwrap()
    }

    #[test]
    fn media_frames_roundtrip_postcard() {
        let f = ServerFrame::Media(Box::new(MediaFrame {
            pane: "P1".into(),
            seq: 7,
            width: 1600,
            height: 1280,
            cell_w: 16,
            cell_h: 32,
            tile_cols: 4,
            tile_rows: 2,
            grid_cols: 25,
            grid_rows: 20,
            reset: true,
            tiles: vec![
                MediaTile {
                    index: 0,
                    col: 0,
                    row: 0,
                    cols: 4,
                    rows: 2,
                    w: 64,
                    h: 64,
                    data: TileData::Shm {
                        name: "/vkb-1".into(),
                        len: 16384,
                    },
                },
                MediaTile {
                    index: 26,
                    col: 4,
                    row: 2,
                    cols: 4,
                    rows: 2,
                    w: 64,
                    h: 64,
                    data: TileData::ZlibRgba(vec![1, 2, 3]),
                },
            ],
        }));
        assert_eq!(rt_server(&f), f);
        let st = ServerFrame::BrowserState {
            pane: "P1".into(),
            state: BrowserStatus {
                url: "http://localhost:5173/".into(),
                env: "laptop chromium → devbox loopback".into(),
                can_back: true,
                ..Default::default()
            },
        };
        assert_eq!(rt_server(&st), st);
        let c = ClientFrame::MediaView {
            panes: vec![MediaPane {
                pane: "P1".into(),
                owner: "devbox".into(),
                spec: BrowserPane {
                    url: "http://localhost:5173/".into(),
                    history: vec!["http://localhost:5173/".into()],
                    ..Default::default()
                },
                cols: 80,
                rows: 23,
                cell_w: 16,
                cell_h: 32,
                dpr: 2.0,
            }],
            shm: true,
            key_releases: true,
        };
        assert_eq!(rt_client(&c), c);
        let k = ClientFrame::Browser {
            input_id: 3,
            pane: "P1".into(),
            cmd: BrowserCmd::Wheel {
                x: 10.5,
                y: 20.0,
                dx: 0.0,
                dy: 40.0,
                mods: crate::input::Mods::empty(),
            },
        };
        assert_eq!(rt_client(&k), k);
    }

    #[test]
    fn event_push_and_watch_frames_roundtrip() {
        let ev = ServerFrame::Events {
            events: vec![PushedEvent {
                seq: 42,
                kind: "client.confirm_requested".into(),
                json: r#"{"seq":42,"type":"client.confirm_requested"}"#.into(),
            }],
            lagged: false,
        };
        assert_eq!(rt_server(&ev), ev);
        let sub = ClientFrame::Subscribe {
            types: vec!["client.confirm_*".into(), "interaction.*".into()],
            after: Some(41),
        };
        assert_eq!(rt_client(&sub), sub);
        let t = ClientFrame::Browser {
            input_id: 1,
            pane: "P".into(),
            cmd: BrowserCmd::TakeOver(true),
        };
        assert_eq!(rt_client(&t), t);
        let st = ServerFrame::BrowserState {
            pane: "P".into(),
            state: BrowserStatus {
                watch: Some("b3".into()),
                human_control: true,
                controlled_here: true,
                ..Default::default()
            },
        };
        assert_eq!(rt_server(&st), st);
        // Appended variants: Events after BrowserState, Subscribe after Browser.
        assert_eq!(encode(&ev).unwrap()[4], 14);
        assert_eq!(encode(&sub).unwrap()[4], 15);
        // ScrollView after Subscribe.
        let sv = ClientFrame::ScrollView {
            pane: "P".into(),
            offset: 12,
            total: 300,
        };
        assert_eq!(rt_client(&sv), sv);
        assert_eq!(encode(&sv).unwrap()[4], 16);
    }

    #[test]
    fn terminal_effect_frames_roundtrip() {
        let q = ServerFrame::ClipboardQuery {
            req: 9,
            pane: "P".into(),
            selection: ClipSel::Clipboard,
        };
        assert_eq!(rt_server(&q), q);
        assert_eq!(encode(&q).unwrap()[4], 15);
        let r = ClientFrame::ClipboardReply {
            req: 9,
            pane: "P".into(),
            data: Some(b"hi".to_vec()),
        };
        assert_eq!(rt_client(&r), r);
        assert_eq!(encode(&r).unwrap()[4], 17);
        let img = ServerFrame::Image {
            hash: "ab".into(),
            width: 2,
            height: 1,
            rgba_z: vec![1, 2, 3],
        };
        assert_eq!(rt_server(&img), img);
        assert_eq!(encode(&img).unwrap()[4], 16);
        let pi = ServerFrame::PaneImages {
            pane: "P".into(),
            epoch: 3,
            places: vec![ImagePlace {
                hash: "ab".into(),
                width: 2,
                height: 1,
                col: -1,
                row: 4,
                cols: 3,
                rows: 2,
                z: 0,
            }],
        };
        assert_eq!(rt_server(&pi), pi);
        assert_eq!(encode(&pi).unwrap()[4], 17);
        let row = Row {
            spans: vec![Span {
                style: Style::default(),
                text: "see docs".into(),
                cols: 8,
            }],
            wrapped: false,
            mark: mark::PROMPT,
            links: vec![Link {
                col: 4,
                cols: 4,
                uri: "https://example.com/".into(),
            }],
        };
        let f = ServerFrame::History {
            pane: "P".into(),
            req: 1,
            start: 0,
            total: 1,
            lines: vec![row.clone()],
        };
        assert_eq!(rt_server(&f), f);
        assert_eq!(row.link_at(5).unwrap().uri, "https://example.com/");
        assert!(row.link_at(3).is_none() && row.link_at(8).is_none());
    }

    /// New variants are appended, so existing postcard discriminants are unchanged.
    #[test]
    fn existing_discriminants_stable() {
        let g = ServerFrame::Goodbye { reason: "x".into() };
        assert_eq!(encode(&g).unwrap()[4], 11);
        assert_eq!(encode(&ClientFrame::Detach).unwrap()[4], 11);
        let m = ClientFrame::MediaAck {
            pane: String::new(),
            seq: 0,
        };
        assert_eq!(encode(&m).unwrap()[4], 13);
    }
}
