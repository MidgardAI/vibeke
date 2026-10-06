//! Host terminal probes for the browser pane (spec 03 §6.1): kitty graphics support and
//! transmission media, cell/window pixel size, DECRQM mode support (SGR-pixels 1016,
//! synchronized output 2026), plus the SGR(-pixels) mouse report parser.
//!
//! The client writes [`probe_sequence`] before entering the alternate screen, reads replies until
//! the DA1 answer (the sentinel: every terminal answers DA1, and answers in order), then folds
//! them with [`GraphicsCaps::from_replies`].

use base64::Engine as _;

/// Probe image ids (arbitrary, distinct per medium).
pub const ID_DIRECT: u32 = 0x7631;
pub const ID_SHM: u32 = 0x7632;
pub const ID_FILE: u32 = 0x7633;

/// `a=q` with a 1×1 RGB pixel sent directly: the terminal answers `OK` (or an error) without
/// storing anything.
pub fn kitty_query_direct(id: u32) -> Vec<u8> {
    format!("\x1b_Gi={id},s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\").into_bytes()
}

/// `a=q` over shared memory: the caller creates `name` holding 3 bytes first
/// ([`crate::kitty::shm::write`]); a terminal that supports `t=s` reads and unlinks it.
pub fn kitty_query_shm(id: u32, name: &str) -> Vec<u8> {
    let p = base64::engine::general_purpose::STANDARD.encode(name);
    format!("\x1b_Gi={id},s=1,v=1,a=q,t=s,f=24,S=3;{p}\x1b\\").into_bytes()
}

/// `a=q` over a temp file (path must contain `tty-graphics-protocol`).
pub fn kitty_query_file(id: u32, path: &str) -> Vec<u8> {
    let p = base64::engine::general_purpose::STANDARD.encode(path);
    format!("\x1b_Gi={id},s=1,v=1,a=q,t=t,f=24,S=3;{p}\x1b\\").into_bytes()
}

/// `CSI 16 t`: cell size in pixels → `CSI 6 ; height ; width t`.
pub const CELL_SIZE_QUERY: &[u8] = b"\x1b[16t";
/// `CSI 14 t`: text area size in pixels → `CSI 4 ; height ; width t`.
pub const WINDOW_SIZE_QUERY: &[u8] = b"\x1b[14t";
/// `CSI 18 t`: text area size in cells → `CSI 8 ; rows ; cols t`.
pub const TEXT_AREA_CELLS_QUERY: &[u8] = b"\x1b[18t";
/// Primary device attributes, used as the "no more answers" sentinel.
pub const DA1: &[u8] = b"\x1b[c";

/// `CSI ? mode $ p` (DECRQM for a private mode).
pub fn decrqm(mode: u32) -> Vec<u8> {
    format!("\x1b[?{mode}$p").into_bytes()
}

/// The whole browser-pane probe batch, ending in DA1. `shm_name`/`file_path` add the medium
/// probes when given (local hosts only).
pub fn probe_sequence(shm_name: Option<&str>, file_path: Option<&str>) -> Vec<u8> {
    let mut v = kitty_query_direct(ID_DIRECT);
    if let Some(n) = shm_name {
        v.extend(kitty_query_shm(ID_SHM, n));
    }
    if let Some(p) = file_path {
        v.extend(kitty_query_file(ID_FILE, p));
    }
    v.extend_from_slice(CELL_SIZE_QUERY);
    v.extend_from_slice(WINDOW_SIZE_QUERY);
    v.extend_from_slice(TEXT_AREA_CELLS_QUERY);
    v.extend(decrqm(1016));
    v.extend(decrqm(2026));
    v.extend_from_slice(DA1);
    v
}

/// DECRPM state (`CSI ? mode ; state $ y`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeState {
    NotRecognized,
    Set,
    Reset,
    PermanentlySet,
    PermanentlyReset,
}

impl ModeState {
    fn from_code(c: u32) -> ModeState {
        match c {
            1 => ModeState::Set,
            2 => ModeState::Reset,
            3 => ModeState::PermanentlySet,
            4 => ModeState::PermanentlyReset,
            _ => ModeState::NotRecognized,
        }
    }

    /// The host knows the mode (it can be enabled).
    pub fn supported(self) -> bool {
        matches!(
            self,
            ModeState::Set | ModeState::Reset | ModeState::PermanentlySet
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    /// `ESC _ G i=<id>[,…] ; <message> ESC \` — `ok` when the message is exactly `OK`.
    Kitty {
        id: Option<u32>,
        ok: bool,
        message: String,
    },
    CellSize {
        width: u32,
        height: u32,
    },
    WindowSize {
        width: u32,
        height: u32,
    },
    TextAreaCells {
        cols: u32,
        rows: u32,
    },
    Mode {
        mode: u32,
        state: ModeState,
    },
    PrimaryDa(Vec<u32>),
}

/// Scan a reply buffer for every recognised answer; other bytes are skipped.
pub fn parse_replies(buf: &[u8]) -> Vec<Reply> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < buf.len() {
        if buf[i] != 0x1b || i + 1 >= buf.len() {
            i += 1;
            continue;
        }
        match buf[i + 1] {
            b'_' => {
                // APC … ST
                let Some(end) = find(&buf[i + 2..], b"\x1b\\") else {
                    break;
                };
                let body = &buf[i + 2..i + 2 + end];
                if let Some(r) = parse_kitty(body) {
                    out.push(r);
                }
                i += 2 + end + 2;
            }
            b'[' => {
                // CSI params final
                let start = i + 2;
                let mut j = start;
                while j < buf.len() && !(0x40..=0x7e).contains(&buf[j]) {
                    j += 1;
                }
                if j >= buf.len() {
                    break;
                }
                if let Some(r) = parse_csi(&buf[start..j], buf[j]) {
                    out.push(r);
                }
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

fn parse_kitty(body: &[u8]) -> Option<Reply> {
    let body = std::str::from_utf8(body).ok()?;
    let body = body.strip_prefix('G')?;
    let (ctrl, msg) = body.split_once(';').unwrap_or((body, ""));
    let id = ctrl
        .split(',')
        .find_map(|kv| kv.strip_prefix("i="))
        .and_then(|v| v.parse().ok());
    Some(Reply::Kitty {
        id,
        ok: msg == "OK",
        message: msg.to_owned(),
    })
}

fn nums(p: &[u8]) -> Option<Vec<u32>> {
    std::str::from_utf8(p)
        .ok()?
        .split(';')
        .map(|s| {
            if s.is_empty() {
                Some(0)
            } else {
                s.parse().ok()
            }
        })
        .collect()
}

fn parse_csi(params: &[u8], fin: u8) -> Option<Reply> {
    match fin {
        b't' => {
            let n = nums(params)?;
            match n.as_slice() {
                [6, h, w] => Some(Reply::CellSize {
                    width: *w,
                    height: *h,
                }),
                [4, h, w] => Some(Reply::WindowSize {
                    width: *w,
                    height: *h,
                }),
                [8, r, c] => Some(Reply::TextAreaCells { cols: *c, rows: *r }),
                _ => None,
            }
        }
        b'y' => {
            // ? mode ; state $
            let p = params.strip_prefix(b"?")?.strip_suffix(b"$")?;
            let n = nums(p)?;
            match n.as_slice() {
                [mode, state] => Some(Reply::Mode {
                    mode: *mode,
                    state: ModeState::from_code(*state),
                }),
                _ => None,
            }
        }
        b'c' => {
            let p = params.strip_prefix(b"?")?;
            Some(Reply::PrimaryDa(nums(p)?))
        }
        _ => None,
    }
}

/// What the browser pane needs from the host, folded from the probe replies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphicsCaps {
    pub kitty_graphics: bool,
    /// `None`: not probed (or no answer before DA1).
    pub kitty_shm: Option<bool>,
    pub kitty_file: Option<bool>,
    pub cell_px: Option<(u32, u32)>,
    pub window_px: Option<(u32, u32)>,
    pub cells: Option<(u32, u32)>,
    pub sgr_pixels: Option<ModeState>,
    pub sync_update: Option<ModeState>,
    /// DA1 arrived (so missing answers mean "unsupported", not "slow").
    pub complete: bool,
}

impl GraphicsCaps {
    pub fn from_replies(replies: &[Reply]) -> GraphicsCaps {
        let mut c = GraphicsCaps::default();
        for r in replies {
            match r {
                Reply::Kitty { id, ok, .. } => match *id {
                    Some(ID_DIRECT) => c.kitty_graphics = *ok,
                    Some(ID_SHM) => c.kitty_shm = Some(*ok),
                    Some(ID_FILE) => c.kitty_file = Some(*ok),
                    _ => {}
                },
                Reply::CellSize { width, height } => c.cell_px = Some((*width, *height)),
                Reply::WindowSize { width, height } => c.window_px = Some((*width, *height)),
                Reply::TextAreaCells { cols, rows } => c.cells = Some((*cols, *rows)),
                Reply::Mode { mode: 1016, state } => c.sgr_pixels = Some(*state),
                Reply::Mode { mode: 2026, state } => c.sync_update = Some(*state),
                Reply::Mode { .. } => {}
                Reply::PrimaryDa(_) => c.complete = true,
            }
        }
        if c.complete && c.kitty_graphics {
            c.kitty_shm.get_or_insert(false);
            c.kitty_file.get_or_insert(false);
        }
        // Derive cell size from window size ÷ cells when CSI 16 t is unanswered.
        if c.cell_px.is_none()
            && let (Some((w, h)), Some((cols, rows))) = (c.window_px, c.cells)
            && cols > 0
            && rows > 0
        {
            c.cell_px = Some((w / cols, h / rows));
        }
        c
    }
}

/// One SGR mouse report (`CSI < b ; x ; y M|m`). With DECSET 1016 (SGR-pixels) `x`/`y` are
/// pixels; otherwise cells. Both are 1-based as reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseReport {
    /// 0 left, 1 middle, 2 right, 3 none (motion), 64.. wheel (64 up, 65 down, 66 left, 67 right),
    /// 128.. extra buttons.
    pub button: u32,
    pub shift: bool,
    pub alt: bool,
    pub ctrl: bool,
    pub motion: bool,
    pub release: bool,
    pub x: u32,
    pub y: u32,
}

impl MouseReport {
    pub fn is_wheel(&self) -> bool {
        (64..128).contains(&self.button)
    }
}

/// Parse one SGR mouse report at the start of `buf`; returns it and the bytes consumed.
pub fn parse_sgr_mouse(buf: &[u8]) -> Option<(MouseReport, usize)> {
    let rest = buf.strip_prefix(b"\x1b[<")?;
    let end = rest.iter().position(|&b| b == b'M' || b == b'm')?;
    let n = nums(&rest[..end])?;
    let [b, x, y] = n.as_slice() else {
        return None;
    };
    let b = *b;
    Some((
        MouseReport {
            // Strip the modifier (4/8/16) and motion (32) bits.
            button: b & !0b0011_1100,
            shift: b & 4 != 0,
            alt: b & 8 != 0,
            ctrl: b & 16 != 0,
            motion: b & 32 != 0,
            release: rest[end] == b'm',
            x: *x,
            y: *y,
        },
        3 + end + 1,
    ))
}

/// Map an SGR-pixels position (1-based host pixels) to CSS pixels inside a browser pane whose
/// top-left content cell is at (`col0`, `row0`) (0-based cells) on a host with `cell_px` cells
/// and device pixel ratio `dpr`.
pub fn pixel_to_css(
    x: u32,
    y: u32,
    col0: u32,
    row0: u32,
    cell_px: (u32, u32),
    dpr: f64,
) -> (f64, f64) {
    let px = x.saturating_sub(1) as f64 - (col0 * cell_px.0) as f64;
    let py = y.saturating_sub(1) as f64 - (row0 * cell_px.1) as f64;
    (px.max(0.0) / dpr, py.max(0.0) / dpr)
}

/// Cell-centre fallback when the host has no SGR-pixels: (1-based cell) → CSS px.
pub fn cell_to_css(
    col: u32,
    row: u32,
    col0: u32,
    row0: u32,
    cell_px: (u32, u32),
    dpr: f64,
) -> (f64, f64) {
    let cx = (col.saturating_sub(1).saturating_sub(col0)) as f64 + 0.5;
    let cy = (row.saturating_sub(1).saturating_sub(row0)) as f64 + 0.5;
    (cx * cell_px.0 as f64 / dpr, cy * cell_px.1 as f64 / dpr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queries() {
        assert_eq!(
            kitty_query_direct(1),
            b"\x1b_Gi=1,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"
        );
        let q = String::from_utf8(kitty_query_shm(2, "/vkb-1")).unwrap();
        assert!(q.contains("t=s"));
        assert!(q.ends_with(";L3ZrYi0x\x1b\\"));
        assert_eq!(decrqm(1016), b"\x1b[?1016$p");
        let seq = probe_sequence(Some("/x"), Some("/tmp/tty-graphics-protocol-x"));
        assert!(seq.ends_with(DA1));
        assert_eq!(seq.windows(3).filter(|w| w == b"a=q").count(), 3);
    }

    #[test]
    fn ghostty_like_replies() {
        // Kitty-capable host: OK for direct and shm, error for file, pixel sizes, DECRPM, DA1.
        let buf =
            b"\x1b_Gi=30257;OK\x1b\\\x1b_Gi=30258;OK\x1b\\\x1b_Gi=30259;EBADF:no such file\x1b\\\
\x1b[6;34;16t\x1b[4;1360;1600t\x1b[8;40;100t\x1b[?1016;2$y\x1b[?2026;2$y\x1b[?62;22;52c";
        let r = parse_replies(buf);
        assert_eq!(r.len(), 9);
        assert_eq!(
            r[2],
            Reply::Kitty {
                id: Some(ID_FILE),
                ok: false,
                message: "EBADF:no such file".into()
            }
        );
        let c = GraphicsCaps::from_replies(&r);
        assert!(c.kitty_graphics);
        assert_eq!(c.kitty_shm, Some(true));
        assert_eq!(c.kitty_file, Some(false));
        assert_eq!(c.cell_px, Some((16, 34)));
        assert_eq!(c.window_px, Some((1600, 1360)));
        assert_eq!(c.cells, Some((100, 40)));
        assert_eq!(c.sgr_pixels, Some(ModeState::Reset));
        assert!(c.sgr_pixels.unwrap().supported());
        assert!(c.complete);
    }

    #[test]
    fn host_without_graphics() {
        // Only DA1 and a "not recognised" DECRPM; no kitty answer, no CSI 16 t.
        let r = parse_replies(b"junk\x1b[?1016;0$y\x1b[4;800;1200t\x1b[8;25;100t\x1b[?1;2c");
        let c = GraphicsCaps::from_replies(&r);
        assert!(!c.kitty_graphics);
        assert_eq!(c.kitty_shm, None);
        assert_eq!(c.sgr_pixels, Some(ModeState::NotRecognized));
        assert!(!c.sgr_pixels.unwrap().supported());
        assert_eq!(c.cell_px, Some((12, 32)), "derived from 14t ÷ 18t");
        assert!(c.complete);
    }

    #[test]
    fn partial_input_is_tolerated() {
        assert!(parse_replies(b"\x1b_Gi=1;OK").is_empty());
        assert!(parse_replies(b"\x1b[6;3").is_empty());
        let r = parse_replies(b"\x1b[6;20;10t\x1b[?62");
        assert_eq!(
            r,
            vec![Reply::CellSize {
                width: 10,
                height: 20
            }]
        );
    }

    #[test]
    fn sgr_mouse() {
        let (m, n) = parse_sgr_mouse(b"\x1b[<0;812;403Mrest").unwrap();
        assert_eq!(n, 13);
        assert_eq!((m.button, m.x, m.y, m.release), (0, 812, 403, false));
        let (m, _) = parse_sgr_mouse(b"\x1b[<2;5;6m").unwrap();
        assert!(m.release);
        assert_eq!(m.button, 2);
        let (m, _) = parse_sgr_mouse(b"\x1b[<65;10;10M").unwrap();
        assert!(m.is_wheel());
        assert_eq!(m.button, 65);
        let (m, _) = parse_sgr_mouse(b"\x1b[<36;1;1M").unwrap(); // shift + motion, left
        assert!(m.shift && m.motion && !m.ctrl);
        assert_eq!(m.button, 0);
        let (m, _) = parse_sgr_mouse(b"\x1b[<88;1;1M").unwrap(); // ctrl+alt wheel up
        assert!(m.ctrl && m.alt && m.is_wheel());
        assert_eq!(m.button, 64);
        assert!(parse_sgr_mouse(b"\x1b[<0;1M").is_none());
        assert!(parse_sgr_mouse(b"\x1b[M").is_none());
    }

    #[test]
    fn coordinate_mapping() {
        // Pane content starts at cell (10, 2); 16×34 px cells; dpr 2.
        let (x, y) = pixel_to_css(161 + 32, 69 + 34, 10, 2, (16, 34), 2.0);
        assert_eq!((x, y), (16.0, 17.0));
        let (x, y) = cell_to_css(11, 3, 10, 2, (16, 34), 2.0);
        assert_eq!((x, y), (4.0, 8.5));
    }
}
