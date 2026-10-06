//! Raw FFI to the vendored libghostty-vt (`vendor/libghostty-vt/include/ghostty/vt/*.h`).
//!
//! Hand-written for exactly the symbols `engine.rs` uses; struct layouts are checked against
//! the library's own ABI manifest (`ghostty_type_json`) by the `ffi_layout` test below. Regenerate
//! or re-check whenever the pin in `vendor/libghostty-vt.vendor.json` moves. Nothing outside
//! `engine.rs` may use this module (spec/03 §2.4).

#![allow(non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_void};

pub type GhosttyResult = c_int;
pub const GHOSTTY_SUCCESS: GhosttyResult = 0;
pub const GHOSTTY_OUT_OF_SPACE: GhosttyResult = -3;
pub const GHOSTTY_NO_VALUE: GhosttyResult = -4;

#[repr(C)]
pub struct GhosttyTerminalImpl {
    _p: [u8; 0],
}
pub type GhosttyTerminal = *mut GhosttyTerminalImpl;
#[repr(C)]
pub struct GhosttySnapshotDecoderImpl {
    _p: [u8; 0],
}
pub type GhosttySnapshotDecoder = *mut GhosttySnapshotDecoderImpl;
#[repr(C)]
pub struct GhosttyTrackedGridRefImpl {
    _p: [u8; 0],
}
pub type GhosttyTrackedGridRef = *mut GhosttyTrackedGridRefImpl;
#[repr(C)]
pub struct GhosttyRenderStateImpl {
    _p: [u8; 0],
}
pub type GhosttyRenderState = *mut GhosttyRenderStateImpl;
#[repr(C)]
pub struct GhosttyRenderStateRowIteratorImpl {
    _p: [u8; 0],
}
pub type GhosttyRenderStateRowIterator = *mut GhosttyRenderStateRowIteratorImpl;
/// Opaque; only ever passed as NULL (default allocator).
#[repr(C)]
pub struct GhosttyAllocator {
    _p: [u8; 0],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyString {
    pub ptr: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct GhosttyColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

// ---- style.h ----
pub const GHOSTTY_STYLE_COLOR_NONE: c_int = 0;
pub const GHOSTTY_STYLE_COLOR_PALETTE: c_int = 1;
pub const GHOSTTY_STYLE_COLOR_RGB: c_int = 2;

#[repr(C)]
#[derive(Clone, Copy)]
pub union GhosttyStyleColorValue {
    pub palette: u8,
    pub rgb: GhosttyColorRgb,
    pub _padding: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyStyleColor {
    pub tag: c_int,
    pub value: GhosttyStyleColorValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyStyle {
    pub size: usize,
    pub fg_color: GhosttyStyleColor,
    pub bg_color: GhosttyStyleColor,
    pub underline_color: GhosttyStyleColor,
    pub bold: bool,
    pub italic: bool,
    pub faint: bool,
    pub blink: bool,
    pub inverse: bool,
    pub invisible: bool,
    pub strikethrough: bool,
    pub overline: bool,
    pub underline: c_int,
}

pub const GHOSTTY_SGR_UNDERLINE_NONE: c_int = 0;
pub const GHOSTTY_SGR_UNDERLINE_SINGLE: c_int = 1;
pub const GHOSTTY_SGR_UNDERLINE_DOUBLE: c_int = 2;
pub const GHOSTTY_SGR_UNDERLINE_CURLY: c_int = 3;
pub const GHOSTTY_SGR_UNDERLINE_DOTTED: c_int = 4;
pub const GHOSTTY_SGR_UNDERLINE_DASHED: c_int = 5;

// ---- screen.h ----
pub type GhosttyCell = u64;
pub type GhosttyRow = u64;

pub const GHOSTTY_CELL_CONTENT_CODEPOINT: c_int = 0;
pub const GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME: c_int = 1;
pub const GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE: c_int = 2;
pub const GHOSTTY_CELL_CONTENT_BG_COLOR_RGB: c_int = 3;

pub const GHOSTTY_CELL_WIDE_NARROW: c_int = 0;
pub const GHOSTTY_CELL_WIDE_WIDE: c_int = 1;
pub const GHOSTTY_CELL_WIDE_SPACER_TAIL: c_int = 2;
pub const GHOSTTY_CELL_WIDE_SPACER_HEAD: c_int = 3;

pub type GhosttyCellData = c_int;
pub const GHOSTTY_CELL_DATA_CODEPOINT: GhosttyCellData = 1;
pub const GHOSTTY_CELL_DATA_CONTENT_TAG: GhosttyCellData = 2;
pub const GHOSTTY_CELL_DATA_WIDE: GhosttyCellData = 3;
pub const GHOSTTY_CELL_DATA_HAS_STYLING: GhosttyCellData = 5;
pub const GHOSTTY_CELL_DATA_STYLE_ID: GhosttyCellData = 6;
pub const GHOSTTY_CELL_DATA_HAS_HYPERLINK: GhosttyCellData = 7;
pub const GHOSTTY_CELL_DATA_SEMANTIC_CONTENT: GhosttyCellData = 9;
pub const GHOSTTY_CELL_DATA_COLOR_PALETTE: GhosttyCellData = 10;
pub const GHOSTTY_CELL_DATA_COLOR_RGB: GhosttyCellData = 11;

pub type GhosttyRowData = c_int;
pub const GHOSTTY_ROW_DATA_WRAP: GhosttyRowData = 1;
pub const GHOSTTY_ROW_DATA_HYPERLINK: GhosttyRowData = 5;
pub const GHOSTTY_ROW_DATA_SEMANTIC_PROMPT: GhosttyRowData = 6;

pub const GHOSTTY_ROW_SEMANTIC_NONE: c_int = 0;
pub const GHOSTTY_ROW_SEMANTIC_PROMPT: c_int = 1;
pub const GHOSTTY_ROW_SEMANTIC_PROMPT_CONTINUATION: c_int = 2;

pub const GHOSTTY_CELL_SEMANTIC_OUTPUT: c_int = 0;
pub const GHOSTTY_CELL_SEMANTIC_INPUT: c_int = 1;
pub const GHOSTTY_CELL_SEMANTIC_PROMPT: c_int = 2;

// ---- point.h / grid_ref.h ----
pub const GHOSTTY_POINT_TAG_ACTIVE: c_int = 0;
pub const GHOSTTY_POINT_TAG_VIEWPORT: c_int = 1;
pub const GHOSTTY_POINT_TAG_SCREEN: c_int = 2;
pub const GHOSTTY_POINT_TAG_HISTORY: c_int = 3;

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct GhosttyPointCoordinate {
    pub x: u16,
    pub y: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union GhosttyPointValue {
    pub coordinate: GhosttyPointCoordinate,
    pub _padding: [u64; 2],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyPoint {
    pub tag: c_int,
    pub value: GhosttyPointValue,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyGridRef {
    pub size: usize,
    pub node: *mut c_void,
    pub x: u16,
    pub y: u16,
}

// ---- modes.h ----
pub type GhosttyMode = u16;
pub const fn ghostty_mode_new(value: u16, ansi: bool) -> GhosttyMode {
    (value & 0x7fff) | ((ansi as u16) << 15)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyTerminalModeConfig {
    pub mode: GhosttyMode,
    pub value: bool,
}

// ---- device.h / size_report.h ----
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributesPrimary {
    pub conformance_level: u16,
    pub features: [u16; 64],
    pub num_features: usize,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributesSecondary {
    pub device_type: u16,
    pub firmware_version: u16,
    pub rom_cartridge: u16,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributesTertiary {
    pub unit_id: u32,
}
#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyDeviceAttributes {
    pub primary: GhosttyDeviceAttributesPrimary,
    pub secondary: GhosttyDeviceAttributesSecondary,
    pub tertiary: GhosttyDeviceAttributesTertiary,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttySizeReportSize {
    pub rows: u16,
    pub columns: u16,
    pub cell_width: u32,
    pub cell_height: u32,
}

// ---- io.h ----
pub type GhosttyWriterFn =
    Option<unsafe extern "C" fn(userdata: *mut c_void, data: *const u8, len: usize) -> bool>;
#[repr(C)]
pub struct GhosttyWriter {
    pub write: GhosttyWriterFn,
    pub userdata: *mut c_void,
}

// ---- terminal.h: effect payloads ----
pub const GHOSTTY_CLIPBOARD_LOCATION_STANDARD: c_int = 0;
pub const GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS: c_int = 0;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyClipboardContent {
    pub mime: GhosttyString,
    pub data: GhosttyString,
}

#[repr(C)]
pub struct GhosttyClipboardWriteReply {
    pub size: usize,
    pub result: c_int,
    pub remember: bool,
}

#[repr(C)]
pub struct GhosttyClipboardWrite {
    pub size: usize,
    pub location: c_int,
    pub contents: *const GhosttyClipboardContent,
    pub contents_len: usize,
    pub name: GhosttyString,
    pub granted: bool,
    pub can_remember: bool,
    pub ctx: *const c_void,
    pub reply: Option<
        unsafe extern "C" fn(
            write: *const GhosttyClipboardWrite,
            reply: *const GhosttyClipboardWriteReply,
        ),
    >,
}

#[repr(C)]
pub struct GhosttyClipboardRead {
    pub size: usize,
    pub location: c_int,
    pub mimes: *const GhosttyString,
    pub mimes_len: usize,
    pub list: bool,
    pub name: GhosttyString,
    pub granted: bool,
    pub can_remember: bool,
    pub ctx: *const c_void,
    pub reply:
        Option<unsafe extern "C" fn(read: *const GhosttyClipboardRead, reply: *const c_void)>,
}

#[repr(C)]
pub struct GhosttyTerminalDesktopNotification {
    pub size: usize,
    pub title: GhosttyString,
    pub body: GhosttyString,
}

#[repr(C)]
pub struct GhosttyTerminalProgressReport {
    pub size: usize,
    pub state: c_int,
    pub progress: i8,
}

pub const GHOSTTY_SEMANTIC_PROMPT_PROMPT_START: c_int = 1;
pub const GHOSTTY_SEMANTIC_PROMPT_INPUT_START: c_int = 2;
pub const GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START: c_int = 3;
pub const GHOSTTY_SEMANTIC_PROMPT_COMMAND_END: c_int = 4;

#[repr(C)]
pub struct GhosttyTerminalSemanticPrompt {
    pub size: usize,
    pub kind: c_int,
    pub prompt_kind: c_int,
    pub has_exit_code: bool,
    pub exit_code: i32,
    pub command: GhosttyString,
    pub error: GhosttyString,
}

pub type BellFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void);
pub type WritePtyFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const u8, usize);
pub type XtversionFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void) -> GhosttyString;
pub type TitleChangedFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void);
pub type SizeFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *mut GhosttySizeReportSize) -> bool;
pub type DeviceAttributesFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *mut GhosttyDeviceAttributes) -> bool;
pub type PwdChangedFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void);
pub type ClipboardWriteFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyClipboardWrite);
pub type ClipboardReadFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyClipboardRead);
pub type DesktopNotificationFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyTerminalDesktopNotification);
pub type ProgressReportFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyTerminalProgressReport);
pub type SemanticPromptFn =
    unsafe extern "C" fn(GhosttyTerminal, *mut c_void, *const GhosttyTerminalSemanticPrompt);
pub type ResetFn = unsafe extern "C" fn(GhosttyTerminal, *mut c_void);

pub type GhosttyTerminalOption = c_int;
pub const GHOSTTY_TERMINAL_OPT_USERDATA: GhosttyTerminalOption = 0;
pub const GHOSTTY_TERMINAL_OPT_WRITE_PTY: GhosttyTerminalOption = 1;
pub const GHOSTTY_TERMINAL_OPT_BELL: GhosttyTerminalOption = 2;
pub const GHOSTTY_TERMINAL_OPT_XTVERSION: GhosttyTerminalOption = 4;
pub const GHOSTTY_TERMINAL_OPT_TITLE_CHANGED: GhosttyTerminalOption = 5;
pub const GHOSTTY_TERMINAL_OPT_SIZE: GhosttyTerminalOption = 6;
pub const GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES: GhosttyTerminalOption = 8;
pub const GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND: GhosttyTerminalOption = 11;
pub const GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND: GhosttyTerminalOption = 12;
pub const GHOSTTY_TERMINAL_OPT_COLOR_CURSOR: GhosttyTerminalOption = 13;
pub const GHOSTTY_TERMINAL_OPT_COLOR_PALETTE: GhosttyTerminalOption = 14;
pub const GHOSTTY_TERMINAL_OPT_PWD_CHANGED: GhosttyTerminalOption = 25;
pub const GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE: GhosttyTerminalOption = 26;
pub const GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES: GhosttyTerminalOption = 27;
pub const GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES: GhosttyTerminalOption = 28;
pub const GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION: GhosttyTerminalOption = 29;
pub const GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT: GhosttyTerminalOption = 30;
pub const GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES: GhosttyTerminalOption = 31;
pub const GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ: GhosttyTerminalOption = 38;
pub const GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT: GhosttyTerminalOption = 42;
pub const GHOSTTY_TERMINAL_OPT_RESET: GhosttyTerminalOption = 43;

pub const GHOSTTY_TERMINAL_SCREEN_PRIMARY: c_int = 0;
pub const GHOSTTY_TERMINAL_SCREEN_ALTERNATE: c_int = 1;

pub type GhosttyTerminalData = c_int;
pub const GHOSTTY_TERMINAL_DATA_COLS: GhosttyTerminalData = 1;
pub const GHOSTTY_TERMINAL_DATA_ROWS: GhosttyTerminalData = 2;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_X: GhosttyTerminalData = 3;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_Y: GhosttyTerminalData = 4;
pub const GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN: GhosttyTerminalData = 6;
pub const GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE: GhosttyTerminalData = 7;
pub const GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS: GhosttyTerminalData = 8;
pub const GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING: GhosttyTerminalData = 11;
pub const GHOSTTY_TERMINAL_DATA_TITLE: GhosttyTerminalData = 12;
pub const GHOSTTY_TERMINAL_DATA_PWD: GhosttyTerminalData = 13;
pub const GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS: GhosttyTerminalData = 15;
pub const GHOSTTY_TERMINAL_DATA_MODE: GhosttyTerminalData = 37;
pub const GHOSTTY_TERMINAL_DATA_VT_GROUND: GhosttyTerminalData = 38;

// ---- snapshot.h ----
pub const GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES: c_int = 0;
pub const GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION: c_int = 1;

// ---- render.h ----
pub const GHOSTTY_RENDER_STATE_DIRTY_FALSE: c_int = 0;
pub const GHOSTTY_RENDER_STATE_DIRTY_PARTIAL: c_int = 1;
pub const GHOSTTY_RENDER_STATE_DIRTY_FULL: c_int = 2;

pub const GHOSTTY_RENDER_STATE_DATA_DIRTY: c_int = 3;
pub const GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR: c_int = 4;
pub const GHOSTTY_RENDER_STATE_DATA_CURSOR: c_int = 18;

pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR: c_int = 0;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK: c_int = 1;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE: c_int = 2;
pub const GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW: c_int = 3;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GhosttyRenderStateCursor {
    pub size: usize,
    pub viewport_has_value: bool,
    pub viewport_x: u16,
    pub viewport_y: u16,
    pub wide_tail: bool,
    pub visible: bool,
    pub blinking: bool,
    pub password_input: bool,
    pub visual_style: c_int,
}

unsafe extern "C" {
    pub fn ghostty_terminal_new(
        allocator: *const GhosttyAllocator,
        terminal: *mut GhosttyTerminal,
        cols: u16,
        rows: u16,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_free(terminal: GhosttyTerminal);
    pub fn ghostty_terminal_resize(
        terminal: GhosttyTerminal,
        cols: u16,
        rows: u16,
        cell_width_px: u32,
        cell_height_px: u32,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_set(
        terminal: GhosttyTerminal,
        option: GhosttyTerminalOption,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_vt_write(terminal: GhosttyTerminal, data: *const u8, len: usize);
    pub fn ghostty_terminal_continuation_buf(
        terminal: GhosttyTerminal,
        buf: *mut u8,
        buf_len: usize,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_get(
        terminal: GhosttyTerminal,
        data: GhosttyTerminalData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_grid_ref(
        terminal: GhosttyTerminal,
        point: GhosttyPoint,
        out_ref: *mut GhosttyGridRef,
    ) -> GhosttyResult;
    pub fn ghostty_terminal_grid_ref_track(
        terminal: GhosttyTerminal,
        point: GhosttyPoint,
        out_ref: *mut GhosttyTrackedGridRef,
    ) -> GhosttyResult;

    pub fn ghostty_tracked_grid_ref_free(r: GhosttyTrackedGridRef);
    pub fn ghostty_tracked_grid_ref_has_value(r: GhosttyTrackedGridRef) -> bool;
    pub fn ghostty_tracked_grid_ref_point(
        r: GhosttyTrackedGridRef,
        tag: c_int,
        out_point: *mut GhosttyPointCoordinate,
    ) -> GhosttyResult;
    pub fn ghostty_tracked_grid_ref_set(
        r: GhosttyTrackedGridRef,
        terminal: GhosttyTerminal,
        point: GhosttyPoint,
    ) -> GhosttyResult;

    pub fn ghostty_grid_ref_cell(
        r: *const GhosttyGridRef,
        out_cell: *mut GhosttyCell,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_row(
        r: *const GhosttyGridRef,
        out_row: *mut GhosttyRow,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_graphemes(
        r: *const GhosttyGridRef,
        buf: *mut u32,
        buf_len: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_hyperlink_uri(
        r: *const GhosttyGridRef,
        buf: *mut u8,
        buf_len: usize,
        out_len: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_grid_ref_style(
        r: *const GhosttyGridRef,
        out_style: *mut GhosttyStyle,
    ) -> GhosttyResult;

    pub fn ghostty_cell_get_multi(
        cell: GhosttyCell,
        count: usize,
        keys: *const GhosttyCellData,
        values: *mut *mut c_void,
        out_written: *mut usize,
    ) -> GhosttyResult;
    pub fn ghostty_cell_get(
        cell: GhosttyCell,
        data: GhosttyCellData,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_row_get(
        row: GhosttyRow,
        data: GhosttyRowData,
        out: *mut c_void,
    ) -> GhosttyResult;

    pub fn ghostty_snapshot_encode(
        terminal: GhosttyTerminal,
        writer: GhosttyWriter,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_new_buf(
        allocator: *const GhosttyAllocator,
        decoder: *mut GhosttySnapshotDecoder,
        ptr: *const u8,
        len: usize,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_free(decoder: GhosttySnapshotDecoder);
    pub fn ghostty_snapshot_decoder_set(
        decoder: GhosttySnapshotDecoder,
        option: c_int,
        value: *const c_void,
    ) -> GhosttyResult;
    pub fn ghostty_snapshot_decoder_decode(
        decoder: GhosttySnapshotDecoder,
        terminal: *mut GhosttyTerminal,
    ) -> GhosttyResult;

    pub fn ghostty_render_state_new(
        allocator: *const GhosttyAllocator,
        state: *mut GhosttyRenderState,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_free(state: GhosttyRenderState);
    pub fn ghostty_render_state_update(
        state: GhosttyRenderState,
        terminal: GhosttyTerminal,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_clean(state: GhosttyRenderState) -> GhosttyResult;
    pub fn ghostty_render_state_get(
        state: GhosttyRenderState,
        data: c_int,
        out: *mut c_void,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_iterator_new(
        allocator: *const GhosttyAllocator,
        out_iterator: *mut GhosttyRenderStateRowIterator,
    ) -> GhosttyResult;
    pub fn ghostty_render_state_row_iterator_free(iterator: GhosttyRenderStateRowIterator);
    pub fn ghostty_render_state_row_iterator_next_dirty(
        iterator: GhosttyRenderStateRowIterator,
        out_y: *mut u16,
    ) -> bool;

    pub fn ghostty_type_json() -> *const c_char;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Checks every struct declared here against the linked library's ABI manifest.
    #[test]
    fn ffi_layout() {
        let json = unsafe { std::ffi::CStr::from_ptr(ghostty_type_json()) };
        let v: serde_json::Value = serde_json::from_slice(json.to_bytes()).unwrap();
        assert_eq!(v["schema"], 1);
        let types = &v["types"];
        let check = |name: &str, size: usize, fields: &[(&str, usize)]| {
            let t = &types[name];
            assert!(!t.is_null(), "{name} missing from manifest");
            assert_eq!(t["size"].as_u64().unwrap() as usize, size, "{name} size");
            for (f, off) in fields {
                let got = t["fields"][f]["offset"].as_u64();
                assert_eq!(got, Some(*off as u64), "{name}.{f} offset");
            }
        };
        macro_rules! layout {
            ($t:ident { $($f:ident),* }) => {
                check(stringify!($t), size_of::<$t>(), &[$((stringify!($f), offset_of!($t, $f))),*])
            };
        }
        layout!(GhosttyString { ptr, len });
        layout!(GhosttyColorRgb { r, g, b });
        layout!(GhosttyStyleColor { tag, value });
        layout!(GhosttyStyle {
            size,
            fg_color,
            bg_color,
            underline_color,
            bold,
            italic,
            faint,
            blink,
            inverse,
            invisible,
            strikethrough,
            overline,
            underline
        });
        layout!(GhosttyPointCoordinate { x, y });
        layout!(GhosttyPoint { tag, value });
        layout!(GhosttyGridRef { size, node, x, y });
        layout!(GhosttyTerminalModeConfig { mode, value });
        layout!(GhosttyDeviceAttributesPrimary {
            conformance_level,
            features,
            num_features
        });
        layout!(GhosttyDeviceAttributesSecondary {
            device_type,
            firmware_version,
            rom_cartridge
        });
        layout!(GhosttyDeviceAttributesTertiary { unit_id });
        layout!(GhosttyDeviceAttributes {
            primary,
            secondary,
            tertiary
        });
        layout!(GhosttySizeReportSize {
            rows,
            columns,
            cell_width,
            cell_height
        });
        layout!(GhosttyWriter { write, userdata });
        layout!(GhosttyClipboardContent { mime, data });
        layout!(GhosttyClipboardWriteReply {
            size,
            result,
            remember
        });
        layout!(GhosttyClipboardWrite {
            size,
            location,
            contents,
            contents_len,
            name,
            granted,
            can_remember,
            ctx,
            reply
        });
        layout!(GhosttyClipboardRead {
            size,
            location,
            mimes,
            mimes_len,
            list,
            name,
            granted,
            can_remember,
            ctx,
            reply
        });
        layout!(GhosttyTerminalDesktopNotification { size, title, body });
        layout!(GhosttyTerminalProgressReport {
            size,
            state,
            progress
        });
        layout!(GhosttyTerminalSemanticPrompt {
            size,
            kind,
            prompt_kind,
            has_exit_code,
            exit_code,
            command,
            error
        });
        layout!(GhosttyRenderStateCursor {
            size,
            viewport_has_value,
            viewport_x,
            viewport_y,
            wide_tail,
            visible,
            blinking,
            password_input,
            visual_style
        });

        // Enum constants: `GHOSTTY_X_Y` must be the manifest value of `Y` under the type's prefix.
        let check_enum = |ty: &str, name: &str, val: i64| {
            let t = &types[ty];
            assert!(!t.is_null(), "{ty} missing from manifest");
            let prefix = t["prefix"].as_str().unwrap();
            let key = name
                .strip_prefix(prefix)
                .unwrap_or_else(|| panic!("{name} vs {prefix}"));
            assert_eq!(t["values"][key].as_i64(), Some(val), "{ty}::{name}");
        };
        macro_rules! enums {
            ($ty:literal: $($c:ident),* $(,)?) => {
                $(check_enum($ty, stringify!($c), $c as i64);)*
            };
        }
        enums!("GhosttyResult": GHOSTTY_SUCCESS, GHOSTTY_OUT_OF_SPACE, GHOSTTY_NO_VALUE);
        enums!("GhosttyStyleColorTag": GHOSTTY_STYLE_COLOR_NONE, GHOSTTY_STYLE_COLOR_PALETTE,
            GHOSTTY_STYLE_COLOR_RGB);
        enums!("GhosttySgrUnderline": GHOSTTY_SGR_UNDERLINE_NONE, GHOSTTY_SGR_UNDERLINE_SINGLE,
            GHOSTTY_SGR_UNDERLINE_DOUBLE, GHOSTTY_SGR_UNDERLINE_CURLY,
            GHOSTTY_SGR_UNDERLINE_DOTTED, GHOSTTY_SGR_UNDERLINE_DASHED);
        enums!("GhosttyCellContentTag": GHOSTTY_CELL_CONTENT_CODEPOINT,
            GHOSTTY_CELL_CONTENT_CODEPOINT_GRAPHEME, GHOSTTY_CELL_CONTENT_BG_COLOR_PALETTE,
            GHOSTTY_CELL_CONTENT_BG_COLOR_RGB);
        enums!("GhosttyCellWide": GHOSTTY_CELL_WIDE_NARROW, GHOSTTY_CELL_WIDE_WIDE,
            GHOSTTY_CELL_WIDE_SPACER_TAIL, GHOSTTY_CELL_WIDE_SPACER_HEAD);
        enums!("GhosttyCellData": GHOSTTY_CELL_DATA_CODEPOINT, GHOSTTY_CELL_DATA_CONTENT_TAG,
            GHOSTTY_CELL_DATA_WIDE, GHOSTTY_CELL_DATA_HAS_STYLING, GHOSTTY_CELL_DATA_STYLE_ID,
            GHOSTTY_CELL_DATA_HAS_HYPERLINK, GHOSTTY_CELL_DATA_SEMANTIC_CONTENT,
            GHOSTTY_CELL_DATA_COLOR_PALETTE, GHOSTTY_CELL_DATA_COLOR_RGB);
        enums!("GhosttyRowData": GHOSTTY_ROW_DATA_WRAP, GHOSTTY_ROW_DATA_HYPERLINK,
            GHOSTTY_ROW_DATA_SEMANTIC_PROMPT);
        enums!("GhosttyRowSemanticPrompt": GHOSTTY_ROW_SEMANTIC_NONE, GHOSTTY_ROW_SEMANTIC_PROMPT,
            GHOSTTY_ROW_SEMANTIC_PROMPT_CONTINUATION);
        enums!("GhosttyCellSemanticContent": GHOSTTY_CELL_SEMANTIC_OUTPUT,
            GHOSTTY_CELL_SEMANTIC_INPUT, GHOSTTY_CELL_SEMANTIC_PROMPT);
        enums!("GhosttyPointTag": GHOSTTY_POINT_TAG_ACTIVE, GHOSTTY_POINT_TAG_VIEWPORT,
            GHOSTTY_POINT_TAG_SCREEN, GHOSTTY_POINT_TAG_HISTORY);
        enums!("GhosttyClipboardLocation": GHOSTTY_CLIPBOARD_LOCATION_STANDARD);
        enums!("GhosttyClipboardWriteResult": GHOSTTY_CLIPBOARD_WRITE_RESULT_SUCCESS);
        enums!("GhosttySemanticPromptKind": GHOSTTY_SEMANTIC_PROMPT_PROMPT_START,
            GHOSTTY_SEMANTIC_PROMPT_INPUT_START, GHOSTTY_SEMANTIC_PROMPT_OUTPUT_START,
            GHOSTTY_SEMANTIC_PROMPT_COMMAND_END);
        enums!("GhosttyTerminalOption": GHOSTTY_TERMINAL_OPT_USERDATA,
            GHOSTTY_TERMINAL_OPT_WRITE_PTY, GHOSTTY_TERMINAL_OPT_BELL,
            GHOSTTY_TERMINAL_OPT_XTVERSION, GHOSTTY_TERMINAL_OPT_TITLE_CHANGED,
            GHOSTTY_TERMINAL_OPT_SIZE, GHOSTTY_TERMINAL_OPT_DEVICE_ATTRIBUTES,
            GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND,
            GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, GHOSTTY_TERMINAL_OPT_COLOR_PALETTE,
            GHOSTTY_TERMINAL_OPT_PWD_CHANGED, GHOSTTY_TERMINAL_OPT_CLIPBOARD_WRITE,
            GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_BYTES, GHOSTTY_TERMINAL_OPT_SCROLLBACK_MAX_LINES,
            GHOSTTY_TERMINAL_OPT_DESKTOP_NOTIFICATION, GHOSTTY_TERMINAL_OPT_PROGRESS_REPORT,
            GHOSTTY_TERMINAL_OPT_CONTINUATION_MAX_BYTES, GHOSTTY_TERMINAL_OPT_CLIPBOARD_READ,
            GHOSTTY_TERMINAL_OPT_SEMANTIC_PROMPT, GHOSTTY_TERMINAL_OPT_RESET);
        enums!("GhosttyTerminalScreen": GHOSTTY_TERMINAL_SCREEN_PRIMARY,
            GHOSTTY_TERMINAL_SCREEN_ALTERNATE);
        enums!("GhosttyTerminalData": GHOSTTY_TERMINAL_DATA_COLS, GHOSTTY_TERMINAL_DATA_ROWS,
            GHOSTTY_TERMINAL_DATA_CURSOR_X, GHOSTTY_TERMINAL_DATA_CURSOR_Y,
            GHOSTTY_TERMINAL_DATA_ACTIVE_SCREEN, GHOSTTY_TERMINAL_DATA_CURSOR_VISIBLE,
            GHOSTTY_TERMINAL_DATA_KITTY_KEYBOARD_FLAGS, GHOSTTY_TERMINAL_DATA_MOUSE_TRACKING,
            GHOSTTY_TERMINAL_DATA_TITLE, GHOSTTY_TERMINAL_DATA_PWD,
            GHOSTTY_TERMINAL_DATA_SCROLLBACK_ROWS, GHOSTTY_TERMINAL_DATA_MODE,
            GHOSTTY_TERMINAL_DATA_VT_GROUND);
        enums!("GhosttySnapshotDecoderOption": GHOSTTY_SNAPSHOT_DECODER_OPT_MAX_CONTINUATION_BYTES,
            GHOSTTY_SNAPSHOT_DECODER_OPT_RETAIN_CONTINUATION);
        enums!("GhosttyRenderStateDirty": GHOSTTY_RENDER_STATE_DIRTY_FALSE,
            GHOSTTY_RENDER_STATE_DIRTY_PARTIAL, GHOSTTY_RENDER_STATE_DIRTY_FULL);
        enums!("GhosttyRenderStateData": GHOSTTY_RENDER_STATE_DATA_DIRTY,
            GHOSTTY_RENDER_STATE_DATA_ROW_ITERATOR, GHOSTTY_RENDER_STATE_DATA_CURSOR);
        enums!("GhosttyRenderStateCursorVisualStyle": GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE,
            GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW);
    }
}
