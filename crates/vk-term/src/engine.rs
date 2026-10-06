//! The one VT engine (03 §1–§2): libghostty-vt, Ghostty's VT core, vendored in
//! `vendor/libghostty-vt` and linked statically by `build.rs`. This file is the only code that
//! touches the FFI in [`crate::ghostty_sys`]; everything else uses [`Engine`].
//!
//! State ownership: the engine owns the grid, modes, palette, title, PWD, kitty keyboard stack,
//! semantic prompts and the unfinished-parser continuation (native snapshot, 03 §2.3). Vibeke
//! keeps only what the engine does not expose, in a small envelope around the engine snapshot:
//! the decoded cwd (emitted as [`Effect::Cwd`]), the modifyOtherKeys level and the monotonic
//! `scrolled_total` counter. [`Tracker`] reads the two sequences Ghostty parses but drops.

use crate::ghostty_sys as sys;
use crate::tracker::{Tracked, Tracker};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};
use vk_proto::render::{Color, Cursor, CursorShape, Link, PaneModes, Row, Span, Style, attr, mark};

pub const ENGINE: &str = "libghostty-vt";
pub const ENGINE_VERSION: &str = concat!("libghostty-vt+", env!("VK_GHOSTTY_SHORT_COMMIT"));

/// Continuation tracking limit (live terminals) and accepted continuation size (decoder). Matches
/// libghostty-vt's largest built-in APC buffer limit so any sequence the engine accepts can be
/// snapshotted mid-way.
const CONTINUATION_MAX: usize = 65 << 20;
/// Pixel cell size reported for XTWINOPS/kitty graphics; the server has no real pixels.
const CELL_PX: (u32, u32) = (8, 16);
const SNAPSHOT_MAGIC: &[u8; 4] = b"VKG1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyKind {
    Osc9,
    Osc99,
    Osc777,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// Bytes to write back to the PTY (query replies).
    Reply(Vec<u8>),
    Bell,
    TitleChanged,
    Notify {
        kind: NotifyKind,
        title: Option<String>,
        body: String,
    },
    Clipboard {
        primary: bool,
        data: Vec<u8>,
    },
    ClipboardQuery {
        primary: bool,
    },
    Cwd(String),
    /// OSC 133 shell-integration mark on the cursor's row.
    Mark {
        kind: char,
        exit: Option<i32>,
    },
    /// OSC 9;4 progress: `state` 0 = remove, 1 = normal, 2 = error, 3 = indeterminate,
    /// 4 = paused/warning.
    Progress {
        state: u8,
        pct: Option<u8>,
    },
    /// OSC 1337 `SetUserVar=name=base64` (iTerm2), decoded. Names are limited to
    /// [`USER_VAR_NAME_MAX`] bytes of `[A-Za-z0-9_.-]`, values to [`USER_VAR_VALUE_MAX`] bytes;
    /// anything else is dropped.
    UserVar {
        name: String,
        value: String,
    },
}

pub const USER_VAR_NAME_MAX: usize = 64;
pub const USER_VAR_VALUE_MAX: usize = 4096;
/// Longest OSC 8 URI kept on a row; longer links render as plain text.
pub const LINK_URI_MAX: usize = 2048;
/// Default largest decoded image a pane may store (03 §9 `graphics.max_image_bytes`): bigger
/// kitty transmissions and PNGs are refused by the engine.
pub const MAX_IMAGE_BYTES: usize = 32 << 20;
/// Default kitty image storage per pane screen (03 §9 `graphics.max_total_per_pane`); the
/// engine evicts the oldest images beyond it.
pub const MAX_IMAGES_PER_PANE: u64 = 256 << 20;
/// Upper bound for `graphics.max_image_bytes`: an image's base64 APC must fit the snapshot
/// continuation limit.
pub const MAX_IMAGE_BYTES_CAP: usize = 48 << 20;

static GRAPHICS_MAX_IMAGE: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(MAX_IMAGE_BYTES);
static GRAPHICS_MAX_TOTAL: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(MAX_IMAGES_PER_PANE);

/// The `[graphics]` limits (03 §9) for engines created or restored from now on. The image
/// size is clamped to 64 KiB ..= [`MAX_IMAGE_BYTES_CAP`]; the per-pane total to at least one
/// image.
pub fn set_graphics_limits(max_image_bytes: u64, max_total_per_pane: u64) {
    use std::sync::atomic::Ordering;
    let (img, total) = clamp_graphics_limits(max_image_bytes, max_total_per_pane);
    GRAPHICS_MAX_IMAGE.store(img, Ordering::Relaxed);
    GRAPHICS_MAX_TOTAL.store(total, Ordering::Relaxed);
}

fn clamp_graphics_limits(max_image_bytes: u64, max_total_per_pane: u64) -> (usize, u64) {
    let img =
        (max_image_bytes.min(usize::MAX as u64) as usize).clamp(64 << 10, MAX_IMAGE_BYTES_CAP);
    (img, max_total_per_pane.max(img as u64))
}

/// The configured largest image (see [`set_graphics_limits`]).
pub fn max_image_bytes() -> usize {
    GRAPHICS_MAX_IMAGE.load(std::sync::atomic::Ordering::Relaxed)
}

/// The configured per-pane image storage.
pub fn max_images_per_pane() -> u64 {
    GRAPHICS_MAX_TOTAL.load(std::sync::atomic::Ordering::Relaxed)
}

/// Images saved with a snapshot are capped at this many bytes in total.
const SNAPSHOT_IMAGES_MAX: usize = 64 << 20;
/// Marks the optional images section after the snapshot header.
const IMAGES_MAGIC: &[u8; 4] = b"VKIM";
/// Largest PNG side accepted by the decoder.
const PNG_MAX_SIDE: u32 = 10_000;

/// Content hashes of cropped images by (image id, generation, source rect).
type ImageHashes = std::collections::HashMap<(u32, u64, [u32; 4]), [u8; 16]>;

/// A visible kitty-graphics placement on the pane screen (03 §9), ready to forward: which
/// pixels (the image cropped to the placement's source rectangle) go into which cells.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    /// Content hash of the cropped RGBA pixels and their size (16 bytes of blake3).
    pub hash: [u8; 16],
    /// Pixel size of the cropped image.
    pub width: u32,
    pub height: u32,
    /// Top-left cell relative to the visible screen; negative when partly scrolled off.
    pub col: i32,
    pub row: i32,
    /// Cells covered.
    pub cols: u32,
    pub rows: u32,
    /// Kitty z-index (negative = below text).
    pub z: i32,
    /// A virtual placement (`U=1`): the program draws unicode placeholder cells for it, so
    /// `col`/`row` are 0 and `cols`/`rows` are the placement's grid size.
    pub virt: bool,
    /// Source rectangle in the stored image and its generation (to fetch the pixels).
    src: (u32, u32, u32, u32),
    generation: u64,
}

/// A kitty image and its placements, kept with a snapshot (03 §2.4: the engine's own
/// snapshot carries placeholder cells but no image state).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SavedImage {
    id: u32,
    width: u32,
    height: u32,
    rgba: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct SavedPlace {
    image_id: u32,
    placement_id: u32,
    virt: bool,
    col: i32,
    row: i32,
    cols: u32,
    rows: u32,
    z: i32,
    src: (u32, u32, u32, u32),
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
struct SavedGraphics {
    images: Vec<SavedImage>,
    places: Vec<SavedPlace>,
}

/// The output of the last command, from OSC 133 marks (`pane.read --source last-command`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastCommand {
    /// Absolute line of the command's prompt row.
    pub prompt_line: u64,
    /// The command's output rows (prompt and command-line rows excluded).
    pub rows: Vec<Row>,
    /// No new prompt yet: the command is still running.
    pub running: bool,
    /// Exit code from `OSC 133 ; D ; code` for a finished command, when the shell sent one.
    pub exit: Option<i32>,
}

/// Default colours; also what OSC 10/11/12/4 queries answer (unless the app overrode them).
#[derive(Clone, Debug)]
pub struct Palette {
    pub fg: (u8, u8, u8),
    pub bg: (u8, u8, u8),
    pub cursor: (u8, u8, u8),
    pub ansi: [(u8, u8, u8); 16],
}

impl Default for Palette {
    fn default() -> Self {
        Palette {
            fg: (0xcd, 0xd6, 0xf4),
            bg: (0x1e, 0x1e, 0x2e),
            cursor: (0xf5, 0xe0, 0xdc),
            ansi: [
                (0x45, 0x47, 0x5a),
                (0xf3, 0x8b, 0xa8),
                (0xa6, 0xe3, 0xa1),
                (0xf9, 0xe2, 0xaf),
                (0x89, 0xb4, 0xfa),
                (0xf5, 0xc2, 0xe7),
                (0x94, 0xe2, 0xd5),
                (0xba, 0xc2, 0xde),
                (0x58, 0x5b, 0x70),
                (0xf3, 0x8b, 0xa8),
                (0xa6, 0xe3, 0xa1),
                (0xf9, 0xe2, 0xaf),
                (0x89, 0xb4, 0xfa),
                (0xf5, 0xc2, 0xe7),
                (0x94, 0xe2, 0xd5),
                (0xa6, 0xad, 0xc8),
            ],
        }
    }
}

/// Vibeke-owned state stored next to the engine snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Extra {
    cwd: Option<String>,
    modify_other_keys: u8,
    scrolled_total: u64,
    hist_last: u64,
}

#[derive(Serialize, Deserialize)]
struct Header {
    engine_version: String,
    extra: Extra,
}

/// Effect-callback state. Heap-allocated so its address (the engine's userdata) is stable;
/// callbacks run synchronously inside `ghostty_terminal_vt_write`.
struct CbState {
    effects: Vec<Effect>,
    cwd: Option<String>,
    /// Inner XTVERSION string (`vibeke X.Y.Z`); the engine adds `DCS > |` … `ST`.
    xtversion: String,
    da: sys::GhosttyDeviceAttributes,
    /// Set by a clipboard read: the engine answers OSC 52 queries with an empty clipboard right
    /// away, but the server answers them itself (as with the old engine), so drop that reply.
    swallow_clip_reply: bool,
}

struct Render {
    state: sys::GhosttyRenderState,
    rows: sys::GhosttyRenderStateRowIterator,
    stale: bool,
}

pub struct Engine {
    term: sys::GhosttyTerminal,
    cb: NonNull<CbState>,
    render: RefCell<Render>,
    /// Tracked reference to the top active row of the primary screen at the end of the last
    /// feed; how far history grew past it gives the lines scrolled since (`scrolled_total`).
    anchor: sys::GhosttyTrackedGridRef,
    scrolled_total: u64,
    /// Raw primary history rows at the last `update_scrolled` (baseline without an anchor).
    hist_last: u64,
    tracker: Tracker,
    tracked: Vec<Tracked>,
    modify_other_keys: u8,
    replaying: bool,
    /// Scrollback rows exposed through `history_*` (the engine keeps a margin more).
    scrollback: usize,
    /// Longest single `vt_write`, so no write can scroll past the retained history.
    write_chunk: usize,
    /// Exit code of the last finished command (`OSC 133 ; D ; code`); cleared when the next
    /// command's output starts. Not part of snapshots.
    last_exit: Option<i32>,
    /// Content hashes of cropped images by (image id, generation, source rect).
    image_hashes: RefCell<ImageHashes>,
    /// DCS tmux passthrough unwrap (`terminal.allow_passthrough`, 03 §8); `None` = off.
    passthrough: Option<crate::passthrough::Unwrap>,
}

// SAFETY: the libghostty-vt handles are plain heap objects without thread affinity; `Engine`
// is used from one thread at a time (it is not `Sync`).
unsafe impl Send for Engine {}

impl Drop for Engine {
    fn drop(&mut self) {
        unsafe {
            if !self.anchor.is_null() {
                sys::ghostty_tracked_grid_ref_free(self.anchor);
            }
            let r = self.render.get_mut();
            sys::ghostty_render_state_row_iterator_free(r.rows);
            sys::ghostty_render_state_free(r.state);
            sys::ghostty_terminal_free(self.term);
            drop(Box::from_raw(self.cb.as_ptr()));
        }
    }
}

fn mode(v: u16) -> sys::GhosttyMode {
    sys::ghostty_mode_new(v, false)
}

fn point(tag: i32, x: u16, y: u32) -> sys::GhosttyPoint {
    sys::GhosttyPoint {
        tag,
        value: sys::GhosttyPointValue {
            coordinate: sys::GhosttyPointCoordinate { x, y },
        },
    }
}

fn rgb(c: (u8, u8, u8)) -> sys::GhosttyColorRgb {
    sys::GhosttyColorRgb {
        r: c.0,
        g: c.1,
        b: c.2,
    }
}

/// Numbers of a `CSI <prefix> Ps ; … c` identification string.
fn ident_params(s: &str, prefix: &str) -> Vec<u16> {
    s.strip_prefix(prefix)
        .and_then(|r| r.strip_suffix('c'))
        .unwrap_or("")
        .split(';')
        .filter_map(|p| p.parse().ok())
        .collect()
}

impl CbState {
    fn new() -> Self {
        let xt = vk_proto::ident::xtversion();
        let xtversion = xt
            .strip_prefix("\x1bP>|")
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .unwrap_or(&xt)
            .to_string();
        let da1 = ident_params(vk_proto::ident::DA1, "\x1b[?");
        let da2 = ident_params(vk_proto::ident::DA2, "\x1b[>");
        let da3 = vk_proto::ident::DA3
            .strip_prefix("\x1bP!|")
            .and_then(|s| s.strip_suffix("\x1b\\"))
            .and_then(|h| u32::from_str_radix(h, 16).ok())
            .unwrap_or(0);
        let mut features = [0u16; 64];
        let feats = da1.get(1..).unwrap_or(&[]);
        features[..feats.len().min(64)].copy_from_slice(&feats[..feats.len().min(64)]);
        CbState {
            effects: Vec::new(),
            cwd: None,
            xtversion,
            da: sys::GhosttyDeviceAttributes {
                primary: sys::GhosttyDeviceAttributesPrimary {
                    conformance_level: da1.first().copied().unwrap_or(62),
                    features,
                    num_features: feats.len().min(64),
                },
                secondary: sys::GhosttyDeviceAttributesSecondary {
                    device_type: da2.first().copied().unwrap_or(1),
                    firmware_version: da2.get(1).copied().unwrap_or(0),
                    rom_cartridge: da2.get(2).copied().unwrap_or(0),
                },
                tertiary: sys::GhosttyDeviceAttributesTertiary { unit_id: da3 },
            },
            swallow_clip_reply: false,
        }
    }
}

// ---- effect callbacks (no panics: they run across the FFI boundary) ----

unsafe fn cb<'a>(ud: *mut c_void) -> &'a mut CbState {
    unsafe { &mut *(ud as *mut CbState) }
}

unsafe fn bytes<'a>(s: sys::GhosttyString) -> &'a [u8] {
    if s.ptr.is_null() || s.len == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(s.ptr, s.len) }
    }
}

unsafe extern "C" fn on_write_pty(
    _t: sys::GhosttyTerminal,
    ud: *mut c_void,
    data: *const u8,
    len: usize,
) {
    let s = unsafe { cb(ud) };
    let b = unsafe { bytes(sys::GhosttyString { ptr: data, len }) };
    if std::mem::take(&mut s.swallow_clip_reply) && b.starts_with(b"\x1b]52;") {
        return;
    }
    s.effects.push(Effect::Reply(b.to_vec()));
}

unsafe extern "C" fn on_bell(_t: sys::GhosttyTerminal, ud: *mut c_void) {
    unsafe { cb(ud) }.effects.push(Effect::Bell);
}

unsafe extern "C" fn on_title(_t: sys::GhosttyTerminal, ud: *mut c_void) {
    unsafe { cb(ud) }.effects.push(Effect::TitleChanged);
}

unsafe extern "C" fn on_reset(_t: sys::GhosttyTerminal, ud: *mut c_void) {
    // RIS clears the title without a title-changed effect.
    unsafe { cb(ud) }.effects.push(Effect::TitleChanged);
}

unsafe extern "C" fn on_xtversion(_t: sys::GhosttyTerminal, ud: *mut c_void) -> sys::GhosttyString {
    let s = unsafe { cb(ud) };
    sys::GhosttyString {
        ptr: s.xtversion.as_ptr(),
        len: s.xtversion.len(),
    }
}

unsafe extern "C" fn on_device_attributes(
    _t: sys::GhosttyTerminal,
    ud: *mut c_void,
    out: *mut sys::GhosttyDeviceAttributes,
) -> bool {
    unsafe { *out = cb(ud).da };
    true
}

unsafe extern "C" fn on_size(
    t: sys::GhosttyTerminal,
    _ud: *mut c_void,
    out: *mut sys::GhosttySizeReportSize,
) -> bool {
    let (mut cols, mut rows) = (0u16, 0u16);
    unsafe {
        sys::ghostty_terminal_get(t, sys::GHOSTTY_TERMINAL_DATA_COLS, (&raw mut cols).cast());
        sys::ghostty_terminal_get(t, sys::GHOSTTY_TERMINAL_DATA_ROWS, (&raw mut rows).cast());
        *out = sys::GhosttySizeReportSize {
            rows,
            columns: cols,
            cell_width: CELL_PX.0,
            cell_height: CELL_PX.1,
        };
    }
    true
}

unsafe extern "C" fn on_pwd(t: sys::GhosttyTerminal, ud: *mut c_void) {
    let s = unsafe { cb(ud) };
    let mut v = sys::GhosttyString {
        ptr: ptr::null(),
        len: 0,
    };
    if unsafe { sys::ghostty_terminal_get(t, sys::GHOSTTY_TERMINAL_DATA_PWD, (&raw mut v).cast()) }
        != sys::GHOSTTY_SUCCESS
    {
        return;
    }
    let path = cwd_from_pwd(&String::from_utf8_lossy(unsafe { bytes(v) }));
    if !path.is_empty() && s.cwd.as_deref() != Some(&path) {
        s.cwd = Some(path.clone());
        s.effects.push(Effect::Cwd(path));
    }
}

unsafe extern "C" fn on_clipboard_write(
    _t: sys::GhosttyTerminal,
    ud: *mut c_void,
    w: *const sys::GhosttyClipboardWrite,
) {
    let s = unsafe { cb(ud) };
    let w = unsafe { &*w };
    let contents: &[sys::GhosttyClipboardContent] = if w.contents.is_null() {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(w.contents, w.contents_len) }
    };
    let pick = contents
        .iter()
        .find(|c| unsafe { bytes(c.mime) }.starts_with(b"text/"))
        .or(contents.first());
    let data = pick
        .map(|c| unsafe { bytes(c.data) }.to_vec())
        .unwrap_or_default();
    s.effects.push(Effect::Clipboard {
        primary: w.location != sys::GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
        data,
    });
    if let Some(reply) = w.reply {
        let r = sys::GhosttyClipboardWriteReply {
            size: size_of::<sys::GhosttyClipboardWriteReply>(),
            result: sys::GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS,
            remember: false,
        };
        unsafe { reply(w, &r) };
    }
}

unsafe extern "C" fn on_clipboard_read(
    _t: sys::GhosttyTerminal,
    ud: *mut c_void,
    r: *const sys::GhosttyClipboardRead,
) {
    let s = unsafe { cb(ud) };
    let r = unsafe { &*r };
    s.effects.push(Effect::ClipboardQuery {
        primary: r.location != sys::GHOSTTY_CLIPBOARD_LOCATION_STANDARD,
    });
    s.swallow_clip_reply = true;
}

unsafe extern "C" fn on_notification(
    _t: sys::GhosttyTerminal,
    ud: *mut c_void,
    n: *const sys::GhosttyTerminalDesktopNotification,
) {
    let s = unsafe { cb(ud) };
    let n = unsafe { &*n };
    let title = String::from_utf8_lossy(unsafe { bytes(n.title) }).into_owned();
    let body = String::from_utf8_lossy(unsafe { bytes(n.body) }).into_owned();
    // OSC 9 carries no title; OSC 777 `notify` does.
    let (kind, title) = if title.is_empty() {
        (NotifyKind::Osc9, None)
    } else {
        (NotifyKind::Osc777, Some(title))
    };
    s.effects.push(Effect::Notify { kind, title, body });
}

unsafe extern "C" fn on_progress(
    _t: sys::GhosttyTerminal,
    ud: *mut c_void,
    p: *const sys::GhosttyTerminalProgressReport,
) {
    let s = unsafe { cb(ud) };
    let p = unsafe { &*p };
    s.effects.push(Effect::Progress {
        state: p.state.clamp(0, 255) as u8,
        pct: (p.progress >= 0).then_some(p.progress as u8),
    });
}

unsafe extern "C" fn on_semantic_prompt(
    _t: sys::GhosttyTerminal,
    ud: *mut c_void,
    e: *const sys::GhosttyTerminalSemanticPrompt,
) {
    let s = unsafe { cb(ud) };
    let e = unsafe { &*e };
    let kind = match e.kind {
        sys::GHOSTTY_SEMANTIC_PROMPT_PROMPT_START => 'A',
        sys::GHOSTTY_SEMANTIC_PROMPT_INPUT_START => 'B',
        sys::GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START => 'C',
        sys::GHOSTTY_SEMANTIC_PROMPT_COMMAND_END => 'D',
        _ => return,
    };
    let exit = (kind == 'D' && e.has_exit_code).then_some(e.exit_code);
    s.effects.push(Effect::Mark { kind, exit });
}

impl Engine {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let (cols, rows) = (cols.max(2), rows.max(1));
        let mut term: sys::GhosttyTerminal = ptr::null_mut();
        let r = unsafe { sys::ghostty_terminal_new(ptr::null(), &mut term, cols, rows) };
        assert!(
            r == sys::GHOSTTY_SUCCESS && !term.is_null(),
            "ghostty_terminal_new failed: {r}"
        );
        // Continuation tracking must be on before the first byte (03 §2.3).
        unsafe {
            set_usize(
                term,
                sys::GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES,
                CONTINUATION_MAX,
            )
        };
        let mut e = Engine::wrap(term, scrollback);
        unsafe { sys::ghostty_terminal_resize(term, cols, rows, CELL_PX.0, CELL_PX.1) };
        e.update_scrolled();
        e
    }

    /// Takes ownership of `term` and installs callbacks, limits and colours.
    fn wrap(term: sys::GhosttyTerminal, scrollback: usize) -> Self {
        let cb = NonNull::from(Box::leak(Box::new(CbState::new())));
        let mut render_state: sys::GhosttyRenderState = ptr::null_mut();
        let mut rows_it: sys::GhosttyRenderStateRowIterator = ptr::null_mut();
        unsafe {
            assert_eq!(
                sys::ghostty_render_state_new(ptr::null(), &mut render_state),
                sys::GHOSTTY_SUCCESS
            );
            assert_eq!(
                sys::ghostty_render_state_row_iterator_new(ptr::null(), &mut rows_it),
                sys::GHOSTTY_SUCCESS
            );
            let set_fn = |opt, f: *const c_void| {
                sys::ghostty_terminal_set(term, opt, f);
            };
            sys::ghostty_terminal_set(term, sys::GHOSTTY_TERMINAL_OPT_USERDATA, cb.as_ptr().cast());
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_WRITE_PTY,
                on_write_pty as sys::WritePtyFn as _,
            );
            set_fn(sys::GHOSTTY_TERMINAL_OPT_BELL, on_bell as sys::BellFn as _);
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_XTVERSION,
                on_xtversion as sys::XtversionFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
                on_title as sys::TitleChangedFn as _,
            );
            set_fn(sys::GHOSTTY_TERMINAL_OPT_SIZE, on_size as sys::SizeFn as _);
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
                on_device_attributes as sys::DeviceAttributesFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_PWD_CHANGED,
                on_pwd as sys::PwdChangedFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
                on_clipboard_write as sys::ClipboardWriteFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ,
                on_clipboard_read as sys::ClipboardReadFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION,
                on_notification as sys::DesktopNotificationFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
                on_progress as sys::ProgressReportFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT,
                on_semantic_prompt as sys::SemanticPromptFn as _,
            );
            set_fn(
                sys::GHOSTTY_TERMINAL_OPT_RESET,
                on_reset as sys::ResetFn as _,
            );
            if scrollback == 0 {
                set_usize(term, sys::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, 0);
            } else {
                // Lines govern; no byte cap. See `set_line_limit`.
                sys::ghostty_terminal_set(
                    term,
                    sys::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES,
                    ptr::null(),
                );
            }
        }
        let mut e = Engine {
            term,
            cb,
            render: RefCell::new(Render {
                state: render_state,
                rows: rows_it,
                stale: true,
            }),
            anchor: ptr::null_mut(),
            scrolled_total: 0,
            hist_last: 0,
            tracker: Tracker::default(),
            tracked: Vec::new(),
            modify_other_keys: 0,
            replaying: false,
            scrollback,
            write_chunk: usize::MAX,
            last_exit: None,
            image_hashes: RefCell::new(std::collections::HashMap::new()),
            passthrough: None,
        };
        e.configure_graphics();
        e.set_line_limit();
        e.set_palette(Palette::default());
        e
    }

    /// libghostty-vt prunes scrollback by whole pages, so the rows it keeps (somewhere between
    /// the limit minus one page and the limit) depend on page layout, which differs between a
    /// live terminal and one restored from a snapshot. To keep history deterministic across
    /// restores, the engine keeps a margin of more than one page and [`Engine::history_len`]
    /// exposes exactly the newest `scrollback` rows.
    fn set_line_limit(&mut self) {
        if self.scrollback == 0 {
            return;
        }
        // A standard page holds 215 x 215 cells; allow for two pages of the current width.
        let cols = (self.cols() as usize).max(1);
        let margin = 2 * (215 * 215) / cols + 64;
        // The engine keeps at least `scrollback + margin - one page` rows.
        self.write_chunk = self.scrollback + margin / 2;
        unsafe {
            set_usize(
                self.term,
                sys::GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
                self.scrollback + margin,
            )
        };
    }

    /// Kitty graphics limits and media (03 §9): direct, shared-memory and temp-file (in the
    /// temp directory only) transmissions; arbitrary file paths (`t=f`) stay off, since the
    /// server would read them on the program's behalf. PNG payloads are decoded by
    /// [`decode_png`].
    fn configure_graphics(&mut self) {
        install_png_decoder();
        let limit: u64 = max_images_per_pane();
        let apc: usize = max_image_bytes() / 3 * 4 + (1 << 16);
        let yes = true;
        let no = false;
        let tmp = std::env::temp_dir().to_string_lossy().into_owned();
        let tmp_s = sys::GhosttyString {
            ptr: tmp.as_ptr(),
            len: tmp.len(),
        };
        unsafe {
            let t = self.term;
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_STORAGE_LIMIT,
                (&raw const limit).cast(),
            );
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_APC_MAX_BYTES_KITTY,
                (&raw const apc).cast(),
            );
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_FILE,
                (&raw const no).cast(),
            );
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_SHARED_MEM,
                (&raw const yes).cast(),
            );
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_KITTY_IMAGE_MEDIUM_TEMP_FILE,
                (&raw const tmp_s).cast(),
            );
        }
    }

    /// Stamp that changes on any kitty transmit, placement or delete on the active screen.
    pub fn images_generation(&self) -> u64 {
        let g: sys::GhosttyKittyGraphics = self.get(sys::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS);
        if g.is_null() {
            return 0;
        }
        let mut gen_ = 0u64;
        unsafe {
            sys::ghostty_kitty_graphics_get(
                g,
                sys::GHOSTTY_KITTY_GRAPHICS_DATA_GENERATION,
                (&raw mut gen_).cast(),
            )
        };
        gen_
    }

    /// Visible, non-virtual kitty placements of the active screen, bottom z first.
    pub fn image_placements(&self) -> Vec<ImagePlacement> {
        let g: sys::GhosttyKittyGraphics = self.get(sys::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS);
        let mut out = Vec::new();
        if g.is_null() {
            return out;
        }
        unsafe {
            let mut it: sys::GhosttyKittyGraphicsPlacementIterator = ptr::null_mut();
            if sys::ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut it)
                != sys::GHOSTTY_SUCCESS
            {
                return out;
            }
            if sys::ghostty_kitty_graphics_get(
                g,
                sys::GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR,
                (&raw mut it).cast(),
            ) == sys::GHOSTTY_SUCCESS
            {
                while sys::ghostty_kitty_graphics_placement_next(it) {
                    if let Some(p) = self.placement(g, it) {
                        out.push(p);
                    }
                }
            }
            sys::ghostty_kitty_graphics_placement_iterator_free(it);
        }
        out.sort_by_key(|p| (p.z, p.row, p.col));
        out
    }

    unsafe fn placement(
        &self,
        g: sys::GhosttyKittyGraphics,
        it: sys::GhosttyKittyGraphicsPlacementIterator,
    ) -> Option<ImagePlacement> {
        let (mut id, mut pid, mut virt, mut z) = (0u32, 0u32, false, 0i32);
        unsafe {
            sys::ghostty_kitty_graphics_placement_get(
                it,
                sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID,
                (&raw mut id).cast(),
            );
            sys::ghostty_kitty_graphics_placement_get(
                it,
                sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_PLACEMENT_ID,
                (&raw mut pid).cast(),
            );
            sys::ghostty_kitty_graphics_placement_get(
                it,
                sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IS_VIRTUAL,
                (&raw mut virt).cast(),
            );
            sys::ghostty_kitty_graphics_placement_get(
                it,
                sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_Z,
                (&raw mut z).cast(),
            );
        }
        if virt {
            return None;
        }
        let img = unsafe { sys::ghostty_kitty_graphics_image(g, id) };
        if img.is_null() {
            return None;
        }
        let mut info = sys::GhosttyKittyGraphicsPlacementRenderInfo {
            size: size_of::<sys::GhosttyKittyGraphicsPlacementRenderInfo>(),
            ..Default::default()
        };
        if unsafe {
            sys::ghostty_kitty_graphics_placement_render_info(it, img, self.term, &mut info)
        } != sys::GHOSTTY_SUCCESS
            || !info.viewport_visible
            || info.source_width == 0
            || info.source_height == 0
        {
            return None;
        }
        let generation = image_u64(img, sys::GHOSTTY_KITTY_IMAGE_DATA_GENERATION);
        let src = (
            info.source_x,
            info.source_y,
            info.source_width,
            info.source_height,
        );
        let hash = self.hash_of(img, id, generation, src)?;
        Some(ImagePlacement {
            image_id: id,
            placement_id: pid,
            hash,
            width: src.2,
            height: src.3,
            col: info.viewport_col,
            row: info.viewport_row,
            cols: info.grid_cols,
            rows: info.grid_rows,
            z,
            virt: false,
            src,
            generation,
        })
    }

    /// Virtual kitty placements (`U=1`) of the active screen: images the program shows through
    /// its own unicode placeholder cells (03 §9). The pixels are the whole image (or the
    /// placement's source rectangle); `cols`/`rows` are the placement's grid size.
    pub fn virtual_placements(&self) -> Vec<ImagePlacement> {
        let g: sys::GhosttyKittyGraphics = self.get(sys::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS);
        let mut out = Vec::new();
        if g.is_null() {
            return out;
        }
        unsafe {
            let mut it: sys::GhosttyKittyGraphicsPlacementIterator = ptr::null_mut();
            if sys::ghostty_kitty_graphics_placement_iterator_new(ptr::null(), &mut it)
                != sys::GHOSTTY_SUCCESS
            {
                return out;
            }
            if sys::ghostty_kitty_graphics_get(
                g,
                sys::GHOSTTY_KITTY_GRAPHICS_DATA_PLACEMENT_ITERATOR,
                (&raw mut it).cast(),
            ) == sys::GHOSTTY_SUCCESS
            {
                while sys::ghostty_kitty_graphics_placement_next(it) {
                    if let Some(p) = self.virtual_placement(g, it) {
                        out.push(p);
                    }
                }
            }
            sys::ghostty_kitty_graphics_placement_iterator_free(it);
        }
        out.sort_by_key(|p| (p.image_id, p.placement_id));
        out
    }

    unsafe fn virtual_placement(
        &self,
        g: sys::GhosttyKittyGraphics,
        it: sys::GhosttyKittyGraphicsPlacementIterator,
    ) -> Option<ImagePlacement> {
        let u32_of = |key: c_int| {
            let mut v = 0u32;
            unsafe { sys::ghostty_kitty_graphics_placement_get(it, key, (&raw mut v).cast()) };
            v
        };
        let mut virt = false;
        unsafe {
            sys::ghostty_kitty_graphics_placement_get(
                it,
                sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IS_VIRTUAL,
                (&raw mut virt).cast(),
            );
        }
        if !virt {
            return None;
        }
        let id = u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_IMAGE_ID);
        let pid = u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_PLACEMENT_ID);
        let cols = u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_COLUMNS);
        let rows = u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_ROWS);
        let img = unsafe { sys::ghostty_kitty_graphics_image(g, id) };
        if img.is_null() {
            return None;
        }
        let (w, h) = (
            image_u32(img, sys::GHOSTTY_KITTY_IMAGE_DATA_WIDTH),
            image_u32(img, sys::GHOSTTY_KITTY_IMAGE_DATA_HEIGHT),
        );
        let (sx, sy) = (
            u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_X),
            u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_Y),
        );
        let sw = match u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_WIDTH) {
            0 => w.saturating_sub(sx),
            v => v,
        };
        let sh = match u32_of(sys::GHOSTTY_KITTY_GRAPHICS_PLACEMENT_DATA_SOURCE_HEIGHT) {
            0 => h.saturating_sub(sy),
            v => v,
        };
        if sw == 0 || sh == 0 {
            return None;
        }
        let src = (sx, sy, sw, sh);
        let generation = image_u64(img, sys::GHOSTTY_KITTY_IMAGE_DATA_GENERATION);
        let hash = self.hash_of(img, id, generation, src)?;
        Some(ImagePlacement {
            image_id: id,
            placement_id: pid,
            hash,
            width: sw,
            height: sh,
            col: 0,
            row: 0,
            cols: cols.max(1),
            rows: rows.max(1),
            z: 0,
            virt: true,
            src,
            generation,
        })
    }

    /// Content hash of an image cropped to `src` (cached by image, generation and rect).
    fn hash_of(
        &self,
        img: sys::GhosttyKittyGraphicsImage,
        id: u32,
        generation: u64,
        src: (u32, u32, u32, u32),
    ) -> Option<[u8; 16]> {
        let key = (id, generation, [src.0, src.1, src.2, src.3]);
        if let Some(h) = self.image_hashes.borrow().get(&key) {
            return Some(*h);
        }
        let px = crop_rgba(img, src)?;
        let mut hs = blake3::Hasher::new();
        hs.update(&src.2.to_le_bytes());
        hs.update(&src.3.to_le_bytes());
        hs.update(&px);
        let mut h = [0u8; 16];
        h.copy_from_slice(&hs.finalize().as_bytes()[..16]);
        let mut c = self.image_hashes.borrow_mut();
        if c.len() > 256 {
            c.clear();
        }
        c.insert(key, h);
        Some(h)
    }

    /// The images and placements of the active screen, for a snapshot (03 §2.4).
    fn saved_graphics(&self) -> SavedGraphics {
        let mut places = self.image_placements();
        places.extend(self.virtual_placements());
        let mut out = SavedGraphics::default();
        if places.is_empty() {
            return out;
        }
        let g: sys::GhosttyKittyGraphics = self.get(sys::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS);
        if g.is_null() {
            return out;
        }
        let mut total = 0usize;
        for p in &places {
            if !out.images.iter().any(|i| i.id == p.image_id) {
                let img = unsafe { sys::ghostty_kitty_graphics_image(g, p.image_id) };
                if img.is_null() {
                    continue;
                }
                let (w, h) = (
                    image_u32(img, sys::GHOSTTY_KITTY_IMAGE_DATA_WIDTH),
                    image_u32(img, sys::GHOSTTY_KITTY_IMAGE_DATA_HEIGHT),
                );
                let Some(rgba) = crop_rgba(img, (0, 0, w, h)) else {
                    continue;
                };
                if total + rgba.len() > SNAPSHOT_IMAGES_MAX {
                    continue;
                }
                total += rgba.len();
                out.images.push(SavedImage {
                    id: p.image_id,
                    width: w,
                    height: h,
                    rgba,
                });
            }
            if out.images.iter().any(|i| i.id == p.image_id) {
                out.places.push(SavedPlace {
                    image_id: p.image_id,
                    placement_id: p.placement_id,
                    virt: p.virt,
                    col: p.col,
                    row: p.row,
                    cols: p.cols,
                    rows: p.rows,
                    z: p.z,
                    src: p.src,
                });
            }
        }
        out
    }

    /// Transmit saved images and placements into a restored engine: images directly (RGBA),
    /// virtual placements as such, visible placements at their screen cell (without moving
    /// the cursor, which is put back afterwards).
    fn restore_graphics(&mut self, g: &SavedGraphics) {
        use base64::Engine as _;
        if g.images.is_empty() {
            return;
        }
        let mut b: Vec<u8> = Vec::new();
        for img in &g.images {
            if img.rgba.len() != img.width as usize * img.height as usize * 4 {
                continue;
            }
            let data = base64::engine::general_purpose::STANDARD.encode(&img.rgba);
            let chunks: Vec<&[u8]> = data.as_bytes().chunks(4096).collect();
            for (i, c) in chunks.iter().enumerate() {
                let more = u8::from(i + 1 < chunks.len());
                if i == 0 {
                    b.extend_from_slice(
                        format!(
                            "\x1b_Ga=t,f=32,s={},v={},i={},q=2,m={more};",
                            img.width, img.height, img.id
                        )
                        .as_bytes(),
                    );
                } else {
                    b.extend_from_slice(format!("\x1b_Gm={more};").as_bytes());
                }
                b.extend_from_slice(c);
                b.extend_from_slice(b"\x1b\\");
            }
        }
        let cur = self.cursor();
        let pid = |p: &SavedPlace| {
            if p.placement_id == 0 {
                String::new()
            } else {
                format!(",p={}", p.placement_id)
            }
        };
        for p in &g.places {
            if p.virt {
                b.extend_from_slice(
                    format!(
                        "\x1b_Ga=p,U=1,i={}{},c={},r={},q=2\x1b\\",
                        p.image_id,
                        pid(p),
                        p.cols,
                        p.rows
                    )
                    .as_bytes(),
                );
            } else if p.row >= 0 && p.col >= 0 {
                b.extend_from_slice(
                    format!(
                        "\x1b[{};{}H\x1b_Ga=p,i={}{},c={},r={},x={},y={},w={},h={},z={},C=1,q=2\x1b\\",
                        p.row + 1,
                        p.col + 1,
                        p.image_id,
                        pid(p),
                        p.cols,
                        p.rows,
                        p.src.0,
                        p.src.1,
                        p.src.2,
                        p.src.3,
                        p.z
                    )
                    .as_bytes(),
                );
            }
        }
        b.extend_from_slice(format!("\x1b[{};{}H", cur.row + 1, cur.col + 1).as_bytes());
        let mut sink = Vec::new();
        let was = self.replaying;
        self.replaying = true;
        self.feed(&b, &mut sink);
        self.replaying = was;
    }

    /// The RGBA pixels of a placement (its image cropped to the source rectangle), if the
    /// image is still stored unchanged.
    pub fn image_rgba(&self, p: &ImagePlacement) -> Option<Vec<u8>> {
        let g: sys::GhosttyKittyGraphics = self.get(sys::GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS);
        if g.is_null() {
            return None;
        }
        let img = unsafe { sys::ghostty_kitty_graphics_image(g, p.image_id) };
        if img.is_null() || image_u64(img, sys::GHOSTTY_KITTY_IMAGE_DATA_GENERATION) != p.generation
        {
            return None;
        }
        crop_rgba(img, p.src)
    }

    fn cb(&mut self) -> &mut CbState {
        // SAFETY: no FFI call is running, so no callback holds the pointer.
        unsafe { self.cb.as_mut() }
    }

    fn cb_ref(&self) -> &CbState {
        unsafe { self.cb.as_ref() }
    }

    pub fn set_palette(&mut self, p: Palette) {
        let mut pal = [sys::GhosttyColorRgb::default(); 256];
        for (i, c) in pal.iter_mut().enumerate() {
            *c = rgb(if i < 16 { p.ansi[i] } else { xterm256(i as u8) });
        }
        unsafe {
            let t = self.term;
            let fg = rgb(p.fg);
            let bg = rgb(p.bg);
            let cur = rgb(p.cursor);
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND,
                (&raw const fg).cast(),
            );
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
                (&raw const bg).cast(),
            );
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_COLOR_CURSOR,
                (&raw const cur).cast(),
            );
            sys::ghostty_terminal_set(
                t,
                sys::GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
                pal.as_ptr().cast(),
            );
        }
    }

    fn get<T: Default>(&self, data: sys::GhosttyTerminalData) -> T {
        let mut v = T::default();
        unsafe { sys::ghostty_terminal_get(self.term, data, (&raw mut v).cast()) };
        v
    }

    fn mode_on(&self, m: sys::GhosttyMode) -> bool {
        let mut c = sys::GhosttyTerminalModeConfig {
            mode: m,
            value: false,
        };
        unsafe {
            sys::ghostty_terminal_get(
                self.term,
                sys::GHOSTTY_TERMINAL_DATA_MODE,
                (&raw mut c).cast(),
            )
        };
        c.value
    }

    fn alt_screen(&self) -> bool {
        self.get::<i32>(sys::GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN)
            == sys::GHOSTTY_TERMINAL_SCREEN_ALTERNATE
    }

    pub fn cols(&self) -> u16 {
        self.get(sys::GHOSTTY_TERMINAL_DATA_COLS)
    }
    pub fn rows(&self) -> u16 {
        self.get(sys::GHOSTTY_TERMINAL_DATA_ROWS)
    }

    /// While replaying the journal after a restart, side effects are suppressed (01 §1.2).
    pub fn set_replaying(&mut self, r: bool) {
        self.replaying = r;
    }
    pub fn replaying(&self) -> bool {
        self.replaying
    }

    /// True between `CSI ? 2026 h` and `l` (synchronized output).
    pub fn in_sync_update(&self) -> bool {
        self.mode_on(mode(2026))
    }

    pub fn cwd(&self) -> Option<&str> {
        self.cb_ref().cwd.as_deref()
    }

    /// `terminal.allow_passthrough` (03 §8): unwrap DCS tmux passthrough so the payload is
    /// processed as if the program wrote it directly.
    pub fn set_allow_passthrough(&mut self, on: bool) {
        match (on, self.passthrough.is_some()) {
            (true, false) => self.passthrough = Some(Default::default()),
            (false, true) => self.passthrough = None,
            _ => {}
        }
    }

    pub fn allow_passthrough(&self) -> bool {
        self.passthrough.is_some()
    }

    pub fn feed(&mut self, bytes: &[u8], out: &mut Vec<Effect>) {
        let unwrapped;
        let bytes = match self.passthrough.as_mut() {
            Some(u) => {
                unwrapped = u.feed(bytes);
                unwrapped.as_slice()
            }
            None => bytes,
        };
        self.tracked.clear();
        self.tracker.feed(bytes, &mut self.tracked);
        for t in std::mem::take(&mut self.tracked) {
            match t {
                Tracked::ModifyOtherKeys(l) => self.modify_other_keys = l,
                Tracked::Reset => self.modify_other_keys = 0,
                Tracked::Osc99(body) => self.cb().effects.push(osc99(&body)),
                Tracked::SetUserVar(body) => {
                    if let Some(e) = user_var(&body) {
                        self.cb().effects.push(e);
                    }
                }
            }
        }
        // Each write scrolls at most one line per byte; keeping writes shorter than the history
        // the engine retains guarantees the scroll anchor survives (see `update_scrolled`).
        for piece in bytes.chunks(self.write_chunk.max(1)) {
            unsafe { sys::ghostty_terminal_vt_write(self.term, piece.as_ptr(), piece.len()) };
            self.update_scrolled();
        }
        self.cb().swallow_clip_reply = false;
        self.render.get_mut().stale = true;
        let mut last_exit = self.last_exit;
        for e in &self.cb_ref().effects {
            if let Effect::Mark { kind, exit } = e {
                match kind {
                    'D' => last_exit = *exit,
                    'C' => last_exit = None,
                    _ => {}
                }
            }
        }
        self.last_exit = last_exit;
        let replaying = self.replaying;
        out.extend(
            self.cb()
                .effects
                .drain(..)
                .filter(|e| !replaying || matches!(e, Effect::TitleChanged | Effect::Cwd(_))),
        );
    }

    /// Advances `scrolled_total` by the primary-screen lines that entered history since the
    /// last call, then re-anchors on the current top active row of the primary screen.
    ///
    /// The anchor is a tracked grid reference: the rows now above it, compared with the history
    /// size, tell how many lines scrolled past it. Writes are chunked so it cannot be pruned
    /// (see `feed`). Without an anchor (fresh restore) the primary history size seen at the last
    /// call (`hist_last`, kept in the snapshot) is the baseline instead.
    fn update_scrolled(&mut self) {
        if self.alt_screen() {
            return; // the primary screen does not scroll meanwhile; keep its anchor
        }
        let top = point(sys::GHOSTTY_POINT_TAG_ACTIVE, 0, 0);
        let hist = self.get::<usize>(sys::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS) as u64;
        unsafe {
            let grown = if self.anchor.is_null() {
                hist.saturating_sub(self.hist_last)
            } else {
                let mut p = sys::GhosttyPointCoordinate::default();
                if sys::ghostty_tracked_grid_ref_has_value(self.anchor)
                    && sys::ghostty_tracked_grid_ref_point(
                        self.anchor,
                        sys::GHOSTTY_POINT_TAG_SCREEN,
                        &mut p,
                    ) == sys::GHOSTTY_SUCCESS
                {
                    hist.saturating_sub(p.y as u64)
                } else {
                    // The anchor row was reset away (RIS, resize): count what history holds.
                    hist
                }
            };
            self.scrolled_total += grown;
            self.hist_last = hist;
            if self.anchor.is_null() {
                sys::ghostty_terminal_grid_ref_track(self.term, top, &mut self.anchor);
            } else {
                sys::ghostty_tracked_grid_ref_set(self.anchor, self.term, top);
            }
        }
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        unsafe {
            sys::ghostty_terminal_resize(self.term, cols.max(2), rows.max(1), CELL_PX.0, CELL_PX.1)
        };
        self.set_line_limit();
        // In-band size reports (mode 2048) raised by the resize have no output channel here.
        self.cb().effects.clear();
        self.update_scrolled();
        self.render.get_mut().stale = true;
    }

    pub fn title(&self) -> String {
        let s: sys::GhosttyString = unsafe {
            let mut v = sys::GhosttyString {
                ptr: ptr::null(),
                len: 0,
            };
            sys::ghostty_terminal_get(
                self.term,
                sys::GHOSTTY_TERMINAL_DATA_TITLE,
                (&raw mut v).cast(),
            );
            v
        };
        String::from_utf8_lossy(unsafe { bytes(s) }).into_owned()
    }

    pub fn modes(&self) -> PaneModes {
        PaneModes {
            alt_screen: self.alt_screen(),
            mouse: self.get(sys::GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING),
            bracketed_paste: self.mode_on(mode(2004)),
            focus_events: self.mode_on(mode(1004)),
            app_cursor: self.mode_on(mode(1)),
            kitty_flags: self.get(sys::GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS),
        }
    }

    /// Bitset of terminal modes, for diagnostics and state comparison (bit layout is Vibeke's:
    /// see `TERM_MODE_BITS`; the old engine returned alacritty's `TermMode` bits).
    pub fn term_mode(&self) -> u32 {
        let mut m = 0;
        for (i, &(v, ansi)) in TERM_MODE_BITS.iter().enumerate() {
            if self.mode_on(sys::ghostty_mode_new(v, ansi)) {
                m |= 1 << i;
            }
        }
        if self.alt_screen() {
            m |= 1 << 31;
        }
        m
    }

    pub fn modify_other_keys(&self) -> u8 {
        self.modify_other_keys
    }

    fn refresh_render(&self) {
        let mut r = self.render.borrow_mut();
        if r.stale {
            unsafe { sys::ghostty_render_state_update(r.state, self.term) };
            r.stale = false;
        }
    }

    pub fn cursor(&self) -> Cursor {
        self.refresh_render();
        let mut c = sys::GhosttyRenderStateCursor {
            size: size_of::<sys::GhosttyRenderStateCursor>(),
            viewport_has_value: false,
            viewport_x: 0,
            viewport_y: 0,
            wide_tail: false,
            visible: true,
            blinking: false,
            password_input: false,
            visual_style: sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK,
        };
        unsafe {
            sys::ghostty_render_state_get(
                self.render.borrow().state,
                sys::GHOSTTY_RENDER_STATE_DATA_CURSOR,
                (&raw mut c).cast(),
            )
        };
        let shape = match c.visual_style {
            sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE => CursorShape::Underline,
            sys::GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR => CursorShape::Bar,
            _ => CursorShape::Block,
        };
        let col: u16 = self.get(sys::GHOSTTY_TERMINAL_DATA_CURSOR_X);
        Cursor {
            col: col.min(self.cols().saturating_sub(1)),
            row: self.get(sys::GHOSTTY_TERMINAL_DATA_CURSOR_Y),
            visible: c.visible,
            shape,
            blink: c.blinking,
        }
    }

    /// Visible row `y` (0 = top) as render spans.
    pub fn row(&self, y: u16) -> Row {
        self.read_row(sys::GHOSTTY_POINT_TAG_ACTIVE, y as u32)
    }

    pub fn visible_rows(&self) -> Vec<Row> {
        (0..self.rows()).map(|y| self.row(y)).collect()
    }

    /// Scrollback rows kept in memory for the primary screen: the newest `scrollback` rows the
    /// engine holds (see `set_line_limit`). While the alternate screen is active this is 0:
    /// libghostty-vt only exposes the active screen's history.
    pub fn history_len(&self) -> usize {
        self.engine_history().min(self.scrollback)
    }

    fn engine_history(&self) -> usize {
        if self.alt_screen() {
            0
        } else {
            self.get(sys::GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS)
        }
    }

    /// Scrollback row `idx` where 0 is the oldest row kept in memory.
    pub fn history_row(&self, idx: usize) -> Option<Row> {
        let all = self.engine_history();
        let len = all.min(self.scrollback);
        if idx >= len {
            return None;
        }
        Some(self.read_row(sys::GHOSTTY_POINT_TAG_HISTORY, (all - len + idx) as u32))
    }

    fn read_row(&self, tag: i32, y: u32) -> Row {
        let mut r = sys::GhosttyGridRef {
            size: size_of::<sys::GhosttyGridRef>(),
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        };
        if unsafe { sys::ghostty_terminal_grid_ref(self.term, point(tag, 0, y), &mut r) }
            != sys::GHOSTTY_SUCCESS
        {
            return Row::default();
        }
        let mut wrapped = false;
        let mut has_links = false;
        let mut sem = 0i32;
        unsafe {
            let mut raw_row: sys::GhosttyRow = 0;
            sys::ghostty_grid_ref_row(&r, &mut raw_row);
            sys::ghostty_row_get(
                raw_row,
                sys::GHOSTTY_ROW_DATA_WRAP,
                (&raw mut wrapped).cast(),
            );
            sys::ghostty_row_get(
                raw_row,
                sys::GHOSTTY_ROW_DATA_HYPERLINK,
                (&raw mut has_links).cast(),
            );
            sys::ghostty_row_get(
                raw_row,
                sys::GHOSTTY_ROW_DATA_SEMANTIC_PROMPT,
                (&raw mut sem).cast(),
            );
        }
        let mut links: Vec<Link> = Vec::new();
        let mut ubuf: Vec<u8> = Vec::new();
        let cols = self.cols();
        let mut spans: Vec<Span> = Vec::new();
        let mut last_style: Option<(u16, Style)> = None;
        let mut gbuf = [0u32; 16];
        for x in 0..cols {
            // Same page node and row: only the column changes (a grid ref is node + x + y).
            r.x = x;
            let mut cell: sys::GhosttyCell = 0;
            unsafe { sys::ghostty_grid_ref_cell(&r, &mut cell) };
            let (mut tag_v, mut wide, mut styled, mut cp, mut sid, mut linked) =
                (0i32, 0i32, false, 0u32, 0u16, false);
            unsafe {
                let keys = [
                    sys::GHOSTTY_CELL_DATA_CONTENT_TAG,
                    sys::GHOSTTY_CELL_DATA_WIDE,
                    sys::GHOSTTY_CELL_DATA_HAS_STYLING,
                    sys::GHOSTTY_CELL_DATA_CODEPOINT,
                    sys::GHOSTTY_CELL_DATA_STYLE_ID,
                    sys::GHOSTTY_CELL_DATA_HAS_HYPERLINK,
                ];
                let mut vals: [*mut c_void; 6] = [
                    (&raw mut tag_v).cast(),
                    (&raw mut wide).cast(),
                    (&raw mut styled).cast(),
                    (&raw mut cp).cast(),
                    (&raw mut sid).cast(),
                    (&raw mut linked).cast(),
                ];
                sys::ghostty_cell_get_multi(
                    cell,
                    if has_links { 6 } else { 5 },
                    keys.as_ptr(),
                    vals.as_mut_ptr(),
                    ptr::null_mut(),
                );
            }
            if wide == sys::GHOSTTY_CELL_WIDE_SPACER_TAIL {
                continue;
            }
            if linked {
                let w = if wide == sys::GHOSTTY_CELL_WIDE_WIDE {
                    2
                } else {
                    1
                };
                if let Some(uri) = cell_uri(&r, &mut ubuf) {
                    match links.last_mut() {
                        Some(l) if l.col + l.cols == x && l.uri == uri => l.cols += w,
                        _ => links.push(Link {
                            col: x,
                            cols: w,
                            uri: uri.to_string(),
                        }),
                    }
                }
            }
            let mut style = if !styled {
                Style::default()
            } else if let Some((id, s)) = last_style
                && id == sid
            {
                s
            } else {
                let s = unsafe {
                    let mut gs: sys::GhosttyStyle = std::mem::zeroed();
                    gs.size = size_of::<sys::GhosttyStyle>();
                    sys::ghostty_grid_ref_style(&r, &mut gs);
                    style_of(&gs)
                };
                last_style = Some((sid, s));
                s
            };
            match tag_v {
                sys::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE => {
                    let mut i = 0u8;
                    unsafe {
                        sys::ghostty_cell_get(
                            cell,
                            sys::GHOSTTY_CELL_DATA_COLOR_PALETTE,
                            (&raw mut i).cast(),
                        )
                    };
                    style.bg = Color::Indexed(i);
                }
                sys::GHOSTTY_CELL_CONTENT_BG_COLOR_RGB => {
                    let mut c = sys::GhosttyColorRgb::default();
                    unsafe {
                        sys::ghostty_cell_get(
                            cell,
                            sys::GHOSTTY_CELL_DATA_COLOR_RGB,
                            (&raw mut c).cast(),
                        )
                    };
                    style.bg = Color::Rgb(c.r, c.g, c.b);
                }
                _ => {}
            }
            let (text, w): (String, u16) = if wide == sys::GHOSTTY_CELL_WIDE_SPACER_HEAD
                || cp == 0
                || tag_v >= sys::GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE
            {
                (" ".into(), 1)
            } else {
                let mut s = String::new();
                if tag_v == sys::GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME {
                    let mut n = 0usize;
                    let res = unsafe {
                        sys::ghostty_grid_ref_graphemes(&r, gbuf.as_mut_ptr(), gbuf.len(), &mut n)
                    };
                    if res == sys::GHOSTTY_SUCCESS {
                        s.extend(gbuf[..n].iter().filter_map(|&c| char::from_u32(c)));
                    } else {
                        let mut big = vec![0u32; n];
                        unsafe { sys::ghostty_grid_ref_graphemes(&r, big.as_mut_ptr(), n, &mut n) };
                        s.extend(
                            big[..n.min(big.len())]
                                .iter()
                                .filter_map(|&c| char::from_u32(c)),
                        );
                    }
                }
                if s.is_empty() {
                    s.push(char::from_u32(cp).unwrap_or('\u{fffd}'));
                }
                (
                    s,
                    if wide == sys::GHOSTTY_CELL_WIDE_WIDE {
                        2
                    } else {
                        1
                    },
                )
            };
            match spans.last_mut() {
                Some(last) if last.style == style => {
                    last.text.push_str(&text);
                    last.cols += w;
                }
                _ => spans.push(Span {
                    style,
                    text,
                    cols: w,
                }),
            }
        }
        // Trim trailing default-styled blanks to keep frames small.
        if let Some(last) = spans.last_mut()
            && last.style == Style::default()
        {
            let trimmed = last.text.trim_end_matches(' ');
            let removed = last.text.len() - trimmed.len();
            if removed > 0 {
                last.cols -= removed as u16;
                last.text.truncate(trimmed.len());
            }
            if last.text.is_empty() {
                spans.pop();
            }
        }
        Row {
            spans,
            wrapped,
            mark: mark_of(sem),
            links,
        }
    }

    /// OSC 133 semantic-prompt state of one row, without reading its cells.
    fn row_mark(&self, tag: i32, y: u32) -> u8 {
        let mut r = sys::GhosttyGridRef {
            size: size_of::<sys::GhosttyGridRef>(),
            node: ptr::null_mut(),
            x: 0,
            y: 0,
        };
        let mut sem = 0i32;
        unsafe {
            if sys::ghostty_terminal_grid_ref(self.term, point(tag, 0, y), &mut r)
                != sys::GHOSTTY_SUCCESS
            {
                return mark::NONE;
            }
            let mut raw_row: sys::GhosttyRow = 0;
            sys::ghostty_grid_ref_row(&r, &mut raw_row);
            sys::ghostty_row_get(
                raw_row,
                sys::GHOSTTY_ROW_DATA_SEMANTIC_PROMPT,
                (&raw mut sem).cast(),
            );
        }
        mark_of(sem)
    }

    /// Marks of every row the engine exposes: history (oldest first) then the visible screen.
    fn all_marks(&self) -> Vec<u8> {
        let all = self.engine_history();
        let len = all.min(self.scrollback);
        let mut v: Vec<u8> = (0..len)
            .map(|i| self.row_mark(sys::GHOSTTY_POINT_TAG_HISTORY, (all - len + i) as u32))
            .collect();
        v.extend((0..self.rows()).map(|y| self.row_mark(sys::GHOSTTY_POINT_TAG_ACTIVE, y as u32)));
        v
    }

    /// Row `i` of the history-then-screen index space used by [`Engine::all_marks`].
    fn any_row(&self, i: usize) -> Row {
        let h = self.history_len();
        if i < h {
            self.history_row(i).unwrap_or_default()
        } else {
            self.row((i - h) as u16)
        }
    }

    /// Absolute line numbers (as in [`Engine::scrolled_total`]) of OSC 133 prompt rows in
    /// memory (history and screen), oldest first. Empty on the alternate screen.
    pub fn prompt_lines(&self) -> Vec<u64> {
        let first = self.scrolled_total - self.history_len() as u64;
        self.all_marks()
            .iter()
            .enumerate()
            .filter(|(_, m)| **m == mark::PROMPT)
            .map(|(i, _)| first + i as u64)
            .collect()
    }

    /// Exit code of the last finished command (`OSC 133 ; D ; code`), if the shell reported
    /// one and no command has started since.
    pub fn last_exit(&self) -> Option<i32> {
        self.last_exit
    }

    /// The last command's output from OSC 133 marks. At an idle prompt (the cursor is on a
    /// prompt row) that is the block between the previous prompt and this one; while a
    /// command runs, the output since the latest prompt. `None` without shell integration.
    pub fn last_command(&self) -> Option<LastCommand> {
        if self.alt_screen() {
            return None;
        }
        let marks = self.all_marks();
        let h = self.history_len();
        let cur = h + self.cursor().row as usize;
        let prompts: Vec<usize> = marks
            .iter()
            .enumerate()
            .filter(|(i, m)| **m == mark::PROMPT && *i <= cur)
            .map(|(i, _)| i)
            .collect();
        let latest = *prompts.last()?;
        let idle = marks.get(cur).is_some_and(|m| *m != mark::NONE);
        let (start, end, running) = if idle {
            let prev = *prompts.iter().rev().nth(1)?;
            (prev, latest, false)
        } else {
            (latest, cur + 1, true)
        };
        // Skip the prompt row and its continuation rows (the command line).
        let mut first = start + 1;
        while first < end && marks[first] != mark::NONE {
            first += 1;
        }
        let mut rows: Vec<Row> = (first..end).map(|i| self.any_row(i)).collect();
        while rows.last().is_some_and(|r| r.text().trim().is_empty()) {
            rows.pop();
        }
        let base = self.scrolled_total - h as u64;
        Some(LastCommand {
            prompt_line: base + start as u64,
            rows,
            running,
            exit: if running { None } else { self.last_exit },
        })
    }

    /// Lines (visible rows) damaged since the last call; `None` means everything.
    pub fn take_damage(&mut self) -> Option<Vec<u16>> {
        self.refresh_render();
        let r = self.render.get_mut();
        let mut dirty = 0i32;
        let mut lines = Vec::new();
        unsafe {
            sys::ghostty_render_state_get(
                r.state,
                sys::GHOSTTY_RENDER_STATE_DATA_DIRTY,
                (&raw mut dirty).cast(),
            );
            if dirty == sys::GHOSTTY_RENDER_STATE_DIRTY_PARTIAL {
                sys::ghostty_render_state_get(
                    r.state,
                    sys::GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR,
                    (&raw mut r.rows).cast(),
                );
                let mut y = 0u16;
                while sys::ghostty_render_state_row_iterator_next_dirty(r.rows, &mut y) {
                    lines.push(y);
                }
            }
            sys::ghostty_render_state_clean(r.state);
        }
        (dirty != sys::GHOSTTY_RENDER_STATE_DIRTY_FULL).then_some(lines)
    }

    /// Lossless serialization: Vibeke envelope + libghostty-vt snapshot stream (which includes
    /// the unfinished parser continuation). Empty if the engine refuses to encode (only when a
    /// single unfinished sequence exceeds the continuation limit); `restore` rejects that.
    pub fn snapshot(&self) -> Vec<u8> {
        let header = Header {
            engine_version: ENGINE_VERSION.into(),
            extra: Extra {
                cwd: self.cb_ref().cwd.clone(),
                modify_other_keys: self.modify_other_keys,
                scrolled_total: self.scrolled_total,
                hist_last: self.hist_last,
            },
        };
        let h = postcard::to_stdvec(&header).expect("snapshot header serializes");
        let mut out = Vec::with_capacity(1 << 16);
        out.extend_from_slice(SNAPSHOT_MAGIC);
        out.extend_from_slice(&(h.len() as u32).to_le_bytes());
        out.extend_from_slice(&h);
        // Optional images section (absent when there are none, so older snapshots and
        // image-free ones read the same).
        let g = self.saved_graphics();
        if !g.images.is_empty()
            && let Ok(gb) = postcard::to_stdvec(&g)
        {
            out.extend_from_slice(IMAGES_MAGIC);
            out.extend_from_slice(&(gb.len() as u32).to_le_bytes());
            out.extend_from_slice(&gb);
        }
        unsafe extern "C" fn write(ud: *mut c_void, data: *const u8, len: usize) -> bool {
            let v = unsafe { &mut *(ud as *mut Vec<u8>) };
            v.extend_from_slice(unsafe { bytes(sys::GhosttyString { ptr: data, len }) });
            true
        }
        let w = sys::GhosttyWriter {
            write: Some(write),
            userdata: (&raw mut out).cast(),
        };
        if unsafe { sys::ghostty_snapshot_encode(self.term, w) } != sys::GHOSTTY_SUCCESS {
            return Vec::new();
        }
        out
    }

    pub fn restore(bytes: &[u8], scrollback: usize) -> anyhow::Result<Self> {
        anyhow::ensure!(
            bytes.len() >= 8 && &bytes[..4] == SNAPSHOT_MAGIC,
            "not a {ENGINE} snapshot"
        );
        let hlen = u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize;
        anyhow::ensure!(bytes.len() >= 8 + hlen, "truncated snapshot header");
        let header: Header = postcard::from_bytes(&bytes[8..8 + hlen])?;
        anyhow::ensure!(
            header.engine_version == ENGINE_VERSION,
            "snapshot from engine {}",
            header.engine_version
        );
        let mut body = &bytes[8 + hlen..];
        let mut graphics = SavedGraphics::default();
        if body.len() >= 8 && &body[..4] == IMAGES_MAGIC {
            let glen = u32::from_le_bytes(body[4..8].try_into().unwrap()) as usize;
            anyhow::ensure!(body.len() >= 8 + glen, "truncated snapshot images");
            graphics = postcard::from_bytes(&body[8..8 + glen]).unwrap_or_default();
            body = &body[8 + glen..];
        }
        let mut term: sys::GhosttyTerminal = ptr::null_mut();
        let res = unsafe {
            let mut dec: sys::GhosttySnapshotDecoder = ptr::null_mut();
            let r = sys::ghostty_snapshot_decoder_new_buf(
                ptr::null(),
                &mut dec,
                body.as_ptr(),
                body.len(),
            );
            anyhow::ensure!(r == sys::GHOSTTY_SUCCESS, "snapshot decoder: {r}");
            let max = CONTINUATION_MAX;
            let retain = true;
            sys::ghostty_snapshot_decoder_set(
                dec,
                sys::GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
                (&raw const max).cast(),
            );
            sys::ghostty_snapshot_decoder_set(
                dec,
                sys::GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION,
                (&raw const retain).cast(),
            );
            let r = sys::ghostty_snapshot_decoder_decode(dec, &mut term);
            sys::ghostty_snapshot_decoder_free(dec);
            r
        };
        anyhow::ensure!(
            res == sys::GHOSTTY_SUCCESS && !term.is_null(),
            "snapshot decode failed: {res}"
        );
        let mut e = Engine::wrap(term, scrollback);
        e.cb().cwd = header.extra.cwd;
        e.modify_other_keys = header.extra.modify_other_keys;
        e.scrolled_total = header.extra.scrolled_total;
        e.hist_last = header.extra.hist_last;
        // Resynchronise the side scanner with the restored parser state.
        let cont = e.continuation();
        let mut sink = Vec::new();
        e.tracker.feed(&cont, &mut sink);
        e.update_scrolled();
        // Images (03 §2.4): back into the engine before the journal replay continues. Only at
        // ground: mid-sequence, the transmissions would land inside the unfinished one.
        if cont.is_empty() {
            e.restore_graphics(&graphics);
        }
        Ok(e)
    }

    fn continuation(&self) -> Vec<u8> {
        let mut n = 0usize;
        unsafe {
            sys::ghostty_terminal_continuation_buf(self.term, ptr::null_mut(), 0, &mut n);
            let mut v = vec![0u8; n];
            if n > 0
                && sys::ghostty_terminal_continuation_buf(self.term, v.as_mut_ptr(), n, &mut n)
                    != sys::GHOSTTY_SUCCESS
            {
                return Vec::new();
            }
            v.truncate(n);
            v
        }
    }

    /// Plain text of the visible screen (trailing spaces trimmed per row).
    pub fn screen_text(&self) -> String {
        self.visible_rows()
            .iter()
            .map(|r| r.text().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Test helper: true when the VT parser is at ground (not inside an escape or UTF-8
    /// sequence), i.e. a snapshot here carries no continuation.
    #[doc(hidden)]
    pub fn tracker_pending_is_empty(&self) -> bool {
        self.get(sys::GHOSTTY_TERMINAL_DATA_VT_GROUND)
    }

    /// Absolute number of primary-screen lines ever scrolled into history. Row `i` of
    /// [`Engine::history_row`] has absolute line number `scrolled_total() - history_len() + i`.
    pub fn scrolled_total(&self) -> u64 {
        self.scrolled_total
    }

    /// Input modes negotiated by the app, for the canonical encoder (03 §7.2).
    pub fn input_modes(&self) -> crate::encode::InputModes {
        use crate::encode::{InputModes, MouseMode};
        let mouse = if self.mode_on(mode(1003)) {
            MouseMode::AnyEvent
        } else if self.mode_on(mode(1002)) {
            MouseMode::ButtonEvent
        } else if self.mode_on(mode(1000)) {
            MouseMode::Normal
        } else if self.mode_on(mode(9)) {
            MouseMode::X10
        } else {
            MouseMode::Off
        };
        InputModes {
            app_cursor: self.mode_on(mode(1)),
            app_keypad: self.mode_on(mode(66)),
            bracketed_paste: self.mode_on(mode(2004)),
            focus_events: self.mode_on(mode(1004)),
            kitty_flags: self.get(sys::GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS),
            modify_other_keys: self.modify_other_keys,
            mouse,
            mouse_sgr: self.mode_on(mode(1006)),
            mouse_utf8: self.mode_on(mode(1005)),
            shift_enter_lf: false,
        }
    }
}

/// Modes reported by [`Engine::term_mode`], bit `i` = entry `i` (`(value, ansi)`); bit 31 is the
/// alternate screen.
const TERM_MODE_BITS: &[(u16, bool)] = &[
    (1, false),    // DECCKM
    (66, false),   // DECNKM
    (2004, false), // bracketed paste
    (1004, false), // focus events
    (9, false),    // X10 mouse
    (1000, false), // normal mouse
    (1002, false), // button-event mouse
    (1003, false), // any-event mouse
    (1005, false), // UTF-8 mouse
    (1006, false), // SGR mouse
    (25, false),   // DECTCEM
    (7, false),    // DECAWM
    (6, false),    // DECOM
    (12, false),   // cursor blink
    (5, false),    // DECSCNM
    (2026, false), // synchronized output
    (2027, false), // grapheme clustering
    (2048, false), // in-band resize
    (1007, false), // alternate scroll
    (4, true),     // IRM
    (20, true),    // LNM
];

unsafe fn set_usize(t: sys::GhosttyTerminal, opt: sys::GhosttyTerminalOption, v: usize) {
    unsafe { sys::ghostty_terminal_set(t, opt, (&raw const v).cast()) };
}

fn style_color(c: &sys::GhosttyStyleColor) -> Color {
    unsafe {
        match c.tag {
            sys::GHOSTTY_STYLE_COLOR_PALETTE => Color::Indexed(c.value.palette),
            sys::GHOSTTY_STYLE_COLOR_RGB => {
                let v = c.value.rgb;
                Color::Rgb(v.r, v.g, v.b)
            }
            _ => Color::Default,
        }
    }
}

fn style_of(s: &sys::GhosttyStyle) -> Style {
    let mut a = 0u16;
    for (on, bit) in [
        (s.bold, attr::BOLD),
        (s.faint, attr::DIM),
        (s.italic, attr::ITALIC),
        (s.inverse, attr::INVERSE),
        (s.invisible, attr::HIDDEN),
        (s.strikethrough, attr::STRIKE),
        (s.blink, attr::BLINK),
    ] {
        if on {
            a |= bit;
        }
    }
    a |= match s.underline {
        sys::GHOSTTY_SGR_UNDERLINE_SINGLE => attr::UNDERLINE,
        sys::GHOSTTY_SGR_UNDERLINE_DOUBLE => attr::DOUBLE_UNDERLINE,
        sys::GHOSTTY_SGR_UNDERLINE_CURLY => attr::UNDERCURL,
        sys::GHOSTTY_SGR_UNDERLINE_DOTTED => attr::DOTTED_UNDERLINE,
        sys::GHOSTTY_SGR_UNDERLINE_DASHED => attr::DASHED_UNDERLINE,
        _ => 0,
    };
    Style {
        fg: style_color(&s.fg_color),
        bg: style_color(&s.bg_color),
        ul: style_color(&s.underline_color),
        attrs: a,
    }
}

/// `OSC 99 ; metadata ; payload` (kitty). Single-chunk title/body, as before.
fn osc99(rest: &[u8]) -> Effect {
    let rest = String::from_utf8_lossy(rest);
    let (meta, payload) = rest.split_once(';').unwrap_or(("", &rest));
    let is_body = meta.split(':').any(|kv| kv == "p=body");
    if is_body {
        Effect::Notify {
            kind: NotifyKind::Osc99,
            title: None,
            body: payload.to_string(),
        }
    } else {
        Effect::Notify {
            kind: NotifyKind::Osc99,
            title: Some(payload.to_string()),
            body: String::new(),
        }
    }
}

/// The engine stores the raw PWD (`file://host/path` from OSC 7, a bare path from OSC 9;9 or
/// OSC 1337 CurrentDir); Vibeke reports a decoded local path.
fn cwd_from_pwd(raw: &str) -> String {
    let path = match raw.split_once("://") {
        Some((_, r)) => r.find('/').map(|i| &r[i..]).unwrap_or(""),
        None => raw,
    };
    percent_decode(path)
}

fn image_u32(img: sys::GhosttyKittyGraphicsImage, key: c_int) -> u32 {
    let mut v = 0u32;
    unsafe { sys::ghostty_kitty_graphics_image_get(img, key, (&raw mut v).cast()) };
    v
}

fn image_u64(img: sys::GhosttyKittyGraphicsImage, key: c_int) -> u64 {
    let mut v = 0u64;
    unsafe { sys::ghostty_kitty_graphics_image_get(img, key, (&raw mut v).cast()) };
    v
}

/// An image's stored pixels (any kitty format) as RGBA, cropped to `src`.
fn crop_rgba(img: sys::GhosttyKittyGraphicsImage, src: (u32, u32, u32, u32)) -> Option<Vec<u8>> {
    let (w, h) = (
        image_u32(img, sys::GHOSTTY_KITTY_IMAGE_DATA_WIDTH),
        image_u32(img, sys::GHOSTTY_KITTY_IMAGE_DATA_HEIGHT),
    );
    let mut fmt = 0i32;
    let mut data: *const u8 = ptr::null();
    let mut len = 0usize;
    unsafe {
        sys::ghostty_kitty_graphics_image_get(
            img,
            sys::GHOSTTY_KITTY_IMAGE_DATA_FORMAT,
            (&raw mut fmt).cast(),
        );
        if sys::ghostty_kitty_graphics_image_get(
            img,
            sys::GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR,
            (&raw mut data).cast(),
        ) != sys::GHOSTTY_SUCCESS
        {
            return None;
        }
        sys::ghostty_kitty_graphics_image_get(
            img,
            sys::GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN,
            (&raw mut len).cast(),
        );
    }
    let bpp = match fmt {
        sys::GHOSTTY_KITTY_IMAGE_FORMAT_RGB => 3,
        sys::GHOSTTY_KITTY_IMAGE_FORMAT_RGBA => 4,
        sys::GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA => 2,
        sys::GHOSTTY_KITTY_IMAGE_FORMAT_GRAY => 1,
        _ => return None,
    };
    if data.is_null() || len < w as usize * h as usize * bpp {
        return None;
    }
    let px = unsafe { std::slice::from_raw_parts(data, len) };
    let (sx, sy, sw, sh) = src;
    if sx.checked_add(sw)? > w || sy.checked_add(sh)? > h {
        return None;
    }
    if sw as usize * sh as usize * 4 > max_image_bytes() {
        return None;
    }
    let mut out = Vec::with_capacity(sw as usize * sh as usize * 4);
    for y in sy..sy + sh {
        let row = &px[(y as usize * w as usize + sx as usize) * bpp..][..sw as usize * bpp];
        for p in row.chunks_exact(bpp) {
            match bpp {
                4 => out.extend_from_slice(p),
                3 => out.extend_from_slice(&[p[0], p[1], p[2], 255]),
                2 => out.extend_from_slice(&[p[0], p[0], p[0], p[1]]),
                _ => out.extend_from_slice(&[p[0], p[0], p[0], 255]),
            }
        }
    }
    Some(out)
}

/// Install the PNG decoder for kitty `f=100` transmissions (process-wide, once).
fn install_png_decoder() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        sys::ghostty_sys_set(
            sys::GHOSTTY_SYS_OPT_DECODE_PNG,
            decode_png as sys::GhosttySysDecodePngFn as *const c_void,
        );
    });
}

/// Decode a PNG into RGBA bytes owned by the engine's allocator. Bounded by
/// [`MAX_IMAGE_BYTES`] and [`PNG_MAX_SIDE`]; never panics across the FFI boundary.
unsafe extern "C" fn decode_png(
    _ud: *mut c_void,
    alloc: *const sys::GhosttyAllocator,
    data: *const u8,
    len: usize,
    out: *mut sys::GhosttySysImage,
) -> bool {
    let input = unsafe { bytes(sys::GhosttyString { ptr: data, len }) };
    let decoded = std::panic::catch_unwind(|| decode_png_rgba(input))
        .ok()
        .flatten();
    let Some((w, h, px)) = decoded else {
        return false;
    };
    let buf = unsafe { sys::ghostty_alloc(alloc, px.len()) };
    if buf.is_null() {
        return false;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(px.as_ptr(), buf, px.len());
        *out = sys::GhosttySysImage {
            width: w,
            height: h,
            data: buf,
            data_len: px.len(),
        };
    }
    true
}

/// PNG bytes -> (width, height, RGBA), within the image limits.
pub fn decode_png_rgba(input: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let mut r =
        image::ImageReader::with_format(std::io::Cursor::new(input), image::ImageFormat::Png);
    let mut lim = image::Limits::default();
    lim.max_image_width = Some(PNG_MAX_SIDE);
    lim.max_image_height = Some(PNG_MAX_SIDE);
    lim.max_alloc = Some(2 * max_image_bytes() as u64);
    r.limits(lim);
    let img = r.decode().ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    let px = img.into_raw();
    (px.len() <= max_image_bytes()).then_some((w, h, px))
}

fn mark_of(sem: i32) -> u8 {
    match sem {
        sys::GHOSTTY_ROW_SEMANTIC_PROMPT => mark::PROMPT,
        sys::GHOSTTY_ROW_SEMANTIC_PROMPT_CONTINUATION => mark::PROMPT_CONT,
        _ => mark::NONE,
    }
}

/// The OSC 8 URI of the cell at `r`, read into `buf`. `None` without a link, or when the URI is
/// longer than [`LINK_URI_MAX`] or not UTF-8.
fn cell_uri<'a>(r: &sys::GhosttyGridRef, buf: &'a mut Vec<u8>) -> Option<&'a str> {
    let mut n = 0usize;
    let res = unsafe { sys::ghostty_grid_ref_hyperlink_uri(r, ptr::null_mut(), 0, &mut n) };
    if !(res == sys::GHOSTTY_SUCCESS || res == sys::GHOSTTY_OUT_OF_SPACE)
        || n == 0
        || n > LINK_URI_MAX
    {
        return None;
    }
    buf.resize(n, 0);
    let res = unsafe { sys::ghostty_grid_ref_hyperlink_uri(r, buf.as_mut_ptr(), n, &mut n) };
    if res != sys::GHOSTTY_SUCCESS {
        return None;
    }
    buf.truncate(n);
    std::str::from_utf8(buf).ok()
}

/// `name=base64` from `OSC 1337 ; SetUserVar=` -> [`Effect::UserVar`] (bounded, sanitized).
fn user_var(body: &[u8]) -> Option<Effect> {
    use base64::Engine as _;
    let s = std::str::from_utf8(body).ok()?;
    let (name, b64) = s.split_once('=')?;
    if name.is_empty()
        || name.len() > USER_VAR_NAME_MAX
        || !name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'))
    {
        return None;
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .ok()?;
    if raw.len() > USER_VAR_VALUE_MAX {
        return None;
    }
    let value: String = String::from_utf8_lossy(&raw)
        .chars()
        .filter(|c| !c.is_control() || *c == '\t')
        .collect();
    Some(Effect::UserVar {
        name: name.to_string(),
        value,
    })
}

fn xterm256(i: u8) -> (u8, u8, u8) {
    if i >= 232 {
        let v = 8 + (i - 232) * 10;
        return (v, v, v);
    }
    let i = i - 16;
    let c = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
    (c(i / 36), c((i / 6) % 6), c(i % 6))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(v) =
                u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16)
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(e: &mut Engine, b: &[u8]) -> Vec<Effect> {
        let mut v = vec![];
        e.feed(b, &mut v);
        v
    }

    #[test]
    fn identification_matches_holder() {
        let mut e = Engine::new(80, 24, 100);
        let r = |s: &str| Effect::Reply(s.as_bytes().to_vec());
        assert_eq!(fx(&mut e, b"\x1b[c"), vec![r(vk_proto::ident::DA1)]);
        assert_eq!(fx(&mut e, b"\x1b[>c"), vec![r(vk_proto::ident::DA2)]);
        assert_eq!(fx(&mut e, b"\x1b[=c"), vec![r(vk_proto::ident::DA3)]);
        assert_eq!(
            fx(&mut e, b"\x1b[>q"),
            vec![r(&vk_proto::ident::xtversion())]
        );
    }

    #[test]
    fn effects() {
        let mut e = Engine::new(80, 24, 100);
        assert_eq!(fx(&mut e, b"\x07"), vec![Effect::Bell]);
        assert_eq!(fx(&mut e, b"\x1b]0;hi\x07"), vec![Effect::TitleChanged]);
        assert_eq!(e.title(), "hi");
        assert_eq!(
            fx(&mut e, b"\x1b]7;file://host/tmp/a%20b\x1b\\"),
            vec![Effect::Cwd("/tmp/a b".into())]
        );
        assert_eq!(e.cwd(), Some("/tmp/a b"));
        assert_eq!(fx(&mut e, b"\x1b]7;file://host/tmp/a%20b\x07"), vec![]);
        assert_eq!(
            fx(&mut e, b"\x1b]9;hello\x07"),
            vec![Effect::Notify {
                kind: NotifyKind::Osc9,
                title: None,
                body: "hello".into()
            }]
        );
        assert_eq!(
            fx(&mut e, b"\x1b]777;notify;T;B\x07"),
            vec![Effect::Notify {
                kind: NotifyKind::Osc777,
                title: Some("T".into()),
                body: "B".into()
            }]
        );
        assert_eq!(
            fx(&mut e, b"\x1b]99;i=1:p=body;yo\x1b\\"),
            vec![Effect::Notify {
                kind: NotifyKind::Osc99,
                title: None,
                body: "yo".into()
            }]
        );
        assert_eq!(
            fx(&mut e, b"\x1b]9;4;1;42\x07"),
            vec![Effect::Progress {
                state: 1,
                pct: Some(42)
            }]
        );
        assert_eq!(
            fx(&mut e, b"\x1b]52;c;aGk=\x07"),
            vec![Effect::Clipboard {
                primary: false,
                data: b"hi".to_vec()
            }]
        );
        assert_eq!(
            fx(&mut e, b"\x1b]52;p;?\x07"),
            vec![Effect::ClipboardQuery { primary: true }]
        );
        assert_eq!(
            fx(&mut e, b"\x1b]133;A\x07\x1b]133;D;3\x07"),
            vec![
                Effect::Mark {
                    kind: 'A',
                    exit: None
                },
                Effect::Mark {
                    kind: 'D',
                    exit: Some(3)
                }
            ]
        );
        let reply = fx(&mut e, b"\x1b]11;?\x07");
        assert_eq!(
            reply,
            vec![Effect::Reply(b"\x1b]11;rgb:1e1e/1e1e/2e2e\x07".to_vec())]
        );
        fx(&mut e, b"\x1b[>4;1m");
        assert_eq!(e.modify_other_keys(), 1);
        fx(&mut e, b"\x1b[?2026h");
        assert!(e.in_sync_update());
    }

    #[test]
    fn rows_styles_and_wide() {
        let mut e = Engine::new(10, 3, 100);
        fx(
            &mut e,
            "\x1b[1;31mab\x1b[0m 日e\u{301}\x1b[44m  \x1b[0m".as_bytes(),
        );
        let r = e.row(0);
        assert_eq!(r.text(), "ab 日e\u{301}  ");
        assert_eq!(r.spans[0].style.fg, Color::Indexed(1));
        assert_eq!(r.spans[0].style.attrs, attr::BOLD);
        assert_eq!(r.spans.iter().map(|s| s.cols).sum::<u16>(), 8);
        assert_eq!(r.spans.last().unwrap().style.bg, Color::Indexed(4));
        fx(&mut e, b"\r\n0123456789xy");
        assert!(e.row(1).wrapped);
        assert_eq!(e.row(2).text(), "xy");
    }

    /// A shell-integrated prompt: `A` prompt `B` command line, `C` output, `D;exit`.
    fn cmd(prompt: &str, line: &str, out: &str, exit: i32) -> String {
        format!(
            "\x1b]133;A\x07{prompt}\x1b]133;B\x07{line}\r\n\x1b]133;C\x07{out}\x1b]133;D;{exit}\x07"
        )
    }

    #[test]
    fn prompt_marks_rows_and_last_command() {
        let mut e = Engine::new(20, 8, 100);
        assert_eq!(e.last_command(), None);
        let mut s = cmd("$ ", "echo one", "one\r\n", 0);
        s += &cmd("$ ", "false", "boom\r\nmore\r\n", 1);
        s += "\x1b]133;A\x07$ \x1b]133;B\x07";
        fx(&mut e, s.as_bytes());
        let rows = e.visible_rows();
        let marks: Vec<u8> = rows.iter().map(|r| r.mark).collect();
        assert_eq!(marks[0], mark::PROMPT, "{rows:?}");
        assert_eq!(marks[1], mark::NONE);
        assert_eq!(marks[2], mark::PROMPT);
        assert_eq!(&marks[3..5], &[mark::NONE, mark::NONE]);
        assert_eq!(marks[5], mark::PROMPT);
        assert_eq!(e.prompt_lines(), vec![0, 2, 5]);
        // Idle at a prompt: the previous command's output.
        let lc = e.last_command().unwrap();
        assert_eq!(lc.prompt_line, 2);
        let text: Vec<String> = lc.rows.iter().map(|r| r.text()).collect();
        assert_eq!(text, vec!["boom", "more"]);
        assert!(!lc.running);
        assert_eq!(lc.exit, Some(1));
        assert_eq!(e.last_exit(), Some(1));
        // A command running: its output so far.
        fx(&mut e, b"sleep 9\r\n\x1b]133;C\x07zzz");
        let lc = e.last_command().unwrap();
        assert!(lc.running);
        assert_eq!(lc.prompt_line, 5);
        assert_eq!(
            lc.rows.iter().map(|r| r.text()).collect::<Vec<_>>(),
            vec!["zzz"]
        );
        assert_eq!(lc.exit, None);
        assert_eq!(e.last_exit(), None);
    }

    #[test]
    fn prompt_lines_follow_scrolling_and_snapshots() {
        let mut e = Engine::new(20, 4, 100);
        let mut s = String::new();
        for i in 0..10 {
            s += &cmd("$ ", &format!("c{i}"), &format!("o{i}\r\n"), 0);
        }
        fx(&mut e, s.as_bytes());
        // Each command takes 2 lines: prompts on even absolute lines.
        let want: Vec<u64> = (0..10).map(|i| 2 * i).collect();
        assert_eq!(e.prompt_lines(), want);
        let first = e.scrolled_total() - e.history_len() as u64;
        let h0 = e.history_row(0).unwrap();
        assert_eq!(h0.mark, mark::PROMPT, "{first} {h0:?}");
        let r = Engine::restore(&e.snapshot(), 100).unwrap();
        assert_eq!(r.prompt_lines(), want);
    }

    #[test]
    fn hyperlinks_on_rows() {
        let mut e = Engine::new(30, 3, 100);
        fx(
            &mut e,
            b"see \x1b]8;;https://example.com/a\x1b\\docs\x1b]8;;\x1b\\ and \x1b]8;id=x;file:///tmp/f\x07f\x1b]8;;\x07",
        );
        let r = e.row(0);
        assert_eq!(r.text(), "see docs and f");
        assert_eq!(
            r.links,
            vec![
                Link {
                    col: 4,
                    cols: 4,
                    uri: "https://example.com/a".into()
                },
                Link {
                    col: 13,
                    cols: 1,
                    uri: "file:///tmp/f".into()
                },
            ]
        );
        assert_eq!(r.link_at(6).unwrap().uri, "https://example.com/a");
        // A link wrapping onto the next row is on both rows.
        let mut e = Engine::new(10, 3, 100);
        fx(&mut e, b"12345\x1b]8;;https://x.y/\x07abcdefgh\x1b]8;;\x07");
        assert_eq!(e.row(0).links[0].col, 5);
        assert_eq!(e.row(0).links[0].cols, 5);
        assert_eq!(e.row(1).links[0].col, 0);
        assert_eq!(e.row(1).links[0].cols, 3);
        // Overlong URIs are dropped (the text stays).
        let long = format!(
            "\x1b]8;;https://x/{}\x07L\x1b]8;;\x07",
            "a".repeat(LINK_URI_MAX)
        );
        let mut e = Engine::new(10, 3, 100);
        fx(&mut e, long.as_bytes());
        assert_eq!(e.row(0).text(), "L");
        assert!(e.row(0).links.is_empty());
    }

    /// Kitty transmit+place of raw pixels: `w`×`h` RGBA at the cursor, `cols`×`rows` cells.
    fn kitty_rgba(id: u32, w: u32, h: u32, px: &[u8], extra: &str) -> Vec<u8> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(px);
        format!("\x1b_Ga=T,i={id},f=32,s={w},v={h},q=2{extra};{b64}\x1b\\").into_bytes()
    }

    #[test]
    fn kitty_placements_and_pixels() {
        let mut e = Engine::new(20, 6, 10);
        assert!(e.image_placements().is_empty());
        let g0 = e.images_generation();
        // 4x2 pixels, red then green rows; placed over 3x2 cells at (2,1).
        let mut px = Vec::new();
        for y in 0..2 {
            for _ in 0..4 {
                px.extend_from_slice(if y == 0 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 255, 0, 255]
                });
            }
        }
        let mut b = b"\x1b[2;3H".to_vec();
        b.extend(kitty_rgba(7, 4, 2, &px, ",c=3,r=2"));
        fx(&mut e, &b);
        assert_ne!(e.images_generation(), g0);
        let ps = e.image_placements();
        assert_eq!(ps.len(), 1, "{ps:?}");
        let p = &ps[0];
        assert_eq!((p.image_id, p.col, p.row, p.cols, p.rows), (7, 2, 1, 3, 2));
        assert_eq!((p.width, p.height), (4, 2));
        assert_eq!(e.image_rgba(p).unwrap(), px);
        // A source rectangle crops: the bottom row only, a different content hash.
        let mut b = b"\x1b[5;1H".to_vec();
        b.extend_from_slice(b"\x1b_Ga=p,i=7,p=2,x=0,y=1,w=4,h=1,c=2,r=1,q=2\x1b\\");
        fx(&mut e, &b);
        let ps = e.image_placements();
        assert_eq!(ps.len(), 2, "{ps:?}");
        let crop = ps.iter().find(|p| p.placement_id == 2).unwrap();
        assert_eq!((crop.width, crop.height, crop.row), (4, 1, 4));
        assert_eq!(e.image_rgba(crop).unwrap(), px[16..].to_vec());
        assert_ne!(crop.hash, p.hash);
        // Scrolling moves placements up; off the top they are gone.
        fx(&mut e, b"\x1b[6;1H\r\n");
        let moved: Vec<i32> = e.image_placements().iter().map(|p| p.row).collect();
        assert!(moved.contains(&0) && moved.contains(&3), "{moved:?}");
        fx(&mut e, b"\r\n\r\n\r\n\r\n\r\n\r\n");
        assert!(e.image_placements().is_empty());
        // Delete.
        let mut e = Engine::new(20, 6, 10);
        fx(&mut e, &kitty_rgba(9, 4, 2, &px, ",c=2,r=1"));
        assert_eq!(e.image_placements().len(), 1);
        fx(&mut e, b"\x1b_Ga=d,d=I,i=9,q=2\x1b\\");
        assert!(e.image_placements().is_empty());
    }

    #[test]
    fn kitty_png_is_decoded_and_oversized_images_refused() {
        // A 2x1 PNG (red, blue) made with the image crate.
        let mut png = Vec::new();
        image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255])
            .unwrap()
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
        let mut e = Engine::new(20, 6, 10);
        let out = fx(
            &mut e,
            format!("\x1b_Ga=T,i=3,f=100,c=2,r=1;{b64}\x1b\\").as_bytes(),
        );
        assert!(
            out.contains(&Effect::Reply(b"\x1b_Gi=3;OK\x1b\\".to_vec())),
            "{out:?}"
        );
        let p = &e.image_placements()[0];
        assert_eq!((p.width, p.height), (2, 1));
        assert_eq!(
            e.image_rgba(p).unwrap(),
            vec![255, 0, 0, 255, 0, 0, 255, 255]
        );
        assert!(decode_png_rgba(b"not a png").is_none());
        // More pixels than the per-image limit: refused, nothing placed.
        let side = 3000u32; // 3000*3000*4 > 32 MiB
        let big = format!("\x1b_Ga=T,i=4,f=32,s={side},v={side},t=d,q=1;AAAA\x1b\\");
        let mut e = Engine::new(20, 6, 10);
        fx(&mut e, big.as_bytes());
        assert!(e.image_placements().is_empty());
        // Arbitrary file paths (t=f) are not read on the program's behalf.
        let path = base64::engine::general_purpose::STANDARD.encode("/etc/hosts");
        let out = fx(
            &mut e,
            format!("\x1b_Ga=T,i=5,f=100,t=f;{path}\x1b\\").as_bytes(),
        );
        assert!(e.image_placements().is_empty());
        assert!(
            out.iter().any(
                |f| matches!(f, Effect::Reply(r) if String::from_utf8_lossy(r).contains("i=5;E"))
            ),
            "{out:?}"
        );
    }

    /// Virtual placements (`U=1`) are listed separately with their grid size and the whole
    /// image (03 §9).
    #[test]
    fn kitty_virtual_placements() {
        let px = vec![7u8; 4 * 2 * 4];
        let mut e = Engine::new(20, 6, 10);
        fx(&mut e, &kitty_rgba(11, 4, 2, &px, ",U=1,c=3,r=2"));
        assert!(e.image_placements().is_empty(), "not a visible placement");
        let vs = e.virtual_placements();
        assert_eq!(vs.len(), 1, "{vs:?}");
        let v = &vs[0];
        assert!(v.virt);
        assert_eq!((v.image_id, v.cols, v.rows), (11, 3, 2));
        assert_eq!((v.width, v.height), (4, 2));
        assert_eq!(e.image_rgba(v).unwrap(), px);
    }

    /// Images and placements survive a snapshot: the restored engine has them again before the
    /// journal replay continues (03 §2.4).
    #[test]
    fn snapshot_keeps_kitty_images_and_placements() {
        let mut px = vec![0u8; 4 * 2 * 4];
        for (i, b) in px.iter_mut().enumerate() {
            *b = i as u8;
        }
        let mut e = Engine::new(20, 6, 10);
        assert!(
            !e.snapshot().windows(4).any(|w| w == IMAGES_MAGIC),
            "no images section without images"
        );
        let mut b = b"\x1b[3;4H".to_vec();
        b.extend(kitty_rgba(7, 4, 2, &px, ",p=5,c=3,r=2"));
        b.extend(kitty_rgba(8, 4, 2, &px, ",U=1,c=2,r=1"));
        b.extend_from_slice(b"\x1b[6;10Hcursor");
        fx(&mut e, &b);
        let before = e.image_placements();
        assert_eq!(before.len(), 1);
        let snap = e.snapshot();
        assert!(snap.windows(4).any(|w| w == IMAGES_MAGIC));
        let r = Engine::restore(&snap, 10).unwrap();
        let after = r.image_placements();
        assert_eq!(after.len(), 1, "{after:?}");
        let (a, p) = (&after[0], &before[0]);
        assert_eq!(
            (
                a.image_id,
                a.placement_id,
                a.col,
                a.row,
                a.cols,
                a.rows,
                a.hash
            ),
            (
                p.image_id,
                p.placement_id,
                p.col,
                p.row,
                p.cols,
                p.rows,
                p.hash
            )
        );
        assert_eq!(r.image_rgba(a).unwrap(), px);
        let v = r.virtual_placements();
        assert_eq!((v.len(), v[0].image_id, v[0].cols), (1, 8, 2));
        assert_eq!(r.cursor().row, e.cursor().row, "cursor put back");
        assert_eq!(r.cursor().col, e.cursor().col);
        assert_eq!(r.screen_text(), e.screen_text());
        // A snapshot without the section (older format) still restores.
        let plain = {
            let mut e2 = Engine::new(20, 6, 10);
            fx(&mut e2, b"hello");
            e2.snapshot()
        };
        assert!(
            Engine::restore(&plain, 10)
                .unwrap()
                .screen_text()
                .contains("hello")
        );
    }

    /// DCS tmux passthrough (03 §8): unwrapped only when allowed.
    #[test]
    fn tmux_passthrough_unwrap() {
        let wrapped = b"\x1bPtmux;\x1b\x1b]2;from tmux\x07\x1b\\";
        let mut e = Engine::new(20, 3, 10);
        assert!(!e.allow_passthrough());
        fx(&mut e, wrapped);
        assert_ne!(e.title(), "from tmux", "ignored by default");
        e.set_allow_passthrough(true);
        fx(&mut e, wrapped);
        assert_eq!(e.title(), "from tmux");
        // A kitty image sent through tmux passthrough is stored and placed.
        let px = vec![1u8; 8];
        let inner = kitty_rgba(3, 2, 1, &px, ",c=1,r=1");
        let mut w = b"\x1bPtmux;".to_vec();
        for &c in &inner {
            if c == 0x1b {
                w.push(0x1b);
            }
            w.push(c);
        }
        w.extend_from_slice(b"\x1b\\");
        // Split across writes.
        let (a, b) = w.split_at(9);
        fx(&mut e, a);
        fx(&mut e, b);
        assert_eq!(e.image_placements().len(), 1);
    }

    #[test]
    fn graphics_limits_are_clamped() {
        assert_eq!(
            clamp_graphics_limits(32 << 20, 256 << 20),
            (32 << 20, 256 << 20)
        );
        assert_eq!(clamp_graphics_limits(1, 1), (64 << 10, 64 << 10));
        assert_eq!(
            clamp_graphics_limits(1 << 40, 1 << 20),
            (MAX_IMAGE_BYTES_CAP, MAX_IMAGE_BYTES_CAP as u64)
        );
        const { assert!(MAX_IMAGE_BYTES_CAP / 3 * 4 + (1 << 16) < CONTINUATION_MAX) };
    }

    #[test]
    fn set_user_var() {
        let mut e = Engine::new(20, 3, 100);
        assert_eq!(
            fx(&mut e, b"\x1b]1337;SetUserVar=branch=bWFpbg==\x07"),
            vec![Effect::UserVar {
                name: "branch".into(),
                value: "main".into()
            }]
        );
        // Bad names, bad base64 and control characters.
        assert_eq!(fx(&mut e, b"\x1b]1337;SetUserVar=a b=eA==\x07"), vec![]);
        assert_eq!(fx(&mut e, b"\x1b]1337;SetUserVar=k=!!\x07"), vec![]);
        assert_eq!(
            fx(&mut e, b"\x1b]1337;SetUserVar=k=YRti\x07"),
            vec![Effect::UserVar {
                name: "k".into(),
                value: "ab".into()
            }]
        );
        // Not replayed.
        e.set_replaying(true);
        assert_eq!(fx(&mut e, b"\x1b]1337;SetUserVar=k=YQ==\x07"), vec![]);
    }

    #[test]
    fn scrolled_total_counts_lines() {
        let mut e = Engine::new(20, 5, 50);
        for i in 0..200 {
            fx(&mut e, format!("line {i}\r\n").as_bytes());
        }
        assert_eq!(e.scrolled_total(), 196);
        let first = e.scrolled_total() - e.history_len() as u64;
        assert_eq!(e.history_row(0).unwrap().text(), format!("line {first}"));
        let mut big = Vec::new();
        for i in 200..400 {
            big.extend(format!("line {i}\r\n").as_bytes());
        }
        fx(&mut e, &big);
        assert_eq!(e.scrolled_total(), 396);
        let r = Engine::restore(&e.snapshot(), 50).unwrap();
        assert_eq!(r.scrolled_total(), 396);
    }
}
