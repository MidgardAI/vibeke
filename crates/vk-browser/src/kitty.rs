//! Kitty graphics protocol output (<https://sw.kovidgoyal.net/kitty/graphics-protocol/>).
//!
//! - Transmission: direct (`t=d`, base64, chunked at 4096 bytes with `m=1`, optional zlib
//!   `o=z`), POSIX shared memory (`t=s`) and temp file (`t=t`). Formats `f=32` (RGBA),
//!   `f=24` (RGB) and `f=100` (PNG).
//! - Placement: virtual placements (`U=1`) shown through unicode placeholder cells
//!   (`U+10EEEE` + row/column/id-MSB diacritics, image id in the foreground colour), so images
//!   clip with the cell grid in splits and under popups.
//!
//! For the browser pane each cell-aligned tile is its own image id with a fixed virtual
//! placement; a changed tile is re-sent with `a=T,U=1` under the same id and the placeholder
//! cells stay as they are.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use base64::Engine as _;

use crate::frame::{Rgba, TileRect};

/// Placeholder character for unicode-placeholder placements.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Maximum base64 payload bytes per escape sequence.
pub const CHUNK: usize = 4096;

/// Row/column diacritics, index = value (kitty `rowcolumn-diacritics.txt`, 297 entries).
pub const DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// `f=32`
    Rgba,
    /// `f=24`
    Rgb,
    /// `f=100`
    Png,
}

impl PixelFormat {
    pub fn code(self) -> u32 {
        match self {
            PixelFormat::Rgba => 32,
            PixelFormat::Rgb => 24,
            PixelFormat::Png => 100,
        }
    }
}

/// Control data for one transmit (`a=t` / `a=T`) or query (`a=q`) command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// `a=`: b'T' transmit+place, b't' transmit only, b'q' query.
    pub action: u8,
    pub id: u32,
    pub format: PixelFormat,
    /// Pixel size (`s=`, `v=`); required for raw formats, ignored for PNG.
    pub width: u32,
    pub height: u32,
    /// `o=z`: payload (raw pixels or PNG bytes) is zlib-deflated.
    pub zlib: bool,
    /// `U=1,c=,r=`: create a virtual placement of `cols × rows` cells.
    pub virtual_cells: Option<(u16, u16)>,
    /// `p=`: placement id, so re-sending a tile replaces its virtual placement instead of
    /// adding another one.
    pub placement: Option<u32>,
    /// `q=`: 0 all replies, 1 errors only, 2 none.
    pub quiet: u8,
}

impl Header {
    pub fn new(id: u32, format: PixelFormat, width: u32, height: u32) -> Header {
        Header {
            action: b'T',
            id,
            format,
            width,
            height,
            zlib: false,
            virtual_cells: None,
            placement: None,
            quiet: 2,
        }
    }

    /// Control keys for medium `t` (`d`, `s`, `t`, `f`), plus `S=` when given.
    pub fn control(&self, medium: char, size: Option<usize>) -> String {
        let mut s = format!(
            "a={},i={},f={},t={medium},q={}",
            self.action as char,
            self.id,
            self.format.code(),
            self.quiet
        );
        if self.format != PixelFormat::Png || self.width > 0 {
            s.push_str(&format!(",s={},v={}", self.width, self.height));
        }
        if self.zlib {
            s.push_str(",o=z");
        }
        if let Some(n) = size {
            s.push_str(&format!(",S={n}"));
        }
        if let Some((c, r)) = self.virtual_cells {
            s.push_str(&format!(",U=1,c={c},r={r}"));
            if let Some(p) = self.placement {
                s.push_str(&format!(",p={p}"));
            }
        }
        s
    }
}

/// zlib-deflate (kitty `o=z` is RFC 1950).
pub fn zlib(data: &[u8], level: u32) -> Vec<u8> {
    let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::new(level));
    e.write_all(data).expect("in-memory write");
    e.finish().expect("in-memory finish")
}

/// Inflate a zlib stream (the client side of `o=z` tiles).
pub fn unzlib(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(data)
        .read_to_end(&mut out)
        .context("inflate")?;
    Ok(out)
}

/// Write `ESC _ G <control> ; <base64 payload> ESC \`, split into chunks of at most
/// [`CHUNK`] base64 bytes. Only the first chunk carries the control keys; every chunk but the
/// last has `m=1`, the last `m=0`. Follow-up chunks repeat `q=` so replies stay suppressed.
pub fn write_chunked(out: &mut Vec<u8>, control: &str, payload: &[u8], quiet: u8) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(payload);
    let b = b64.as_bytes();
    if b.len() <= CHUNK {
        out.extend_from_slice(b"\x1b_G");
        out.extend_from_slice(control.as_bytes());
        out.push(b';');
        out.extend_from_slice(b);
        out.extend_from_slice(b"\x1b\\");
        return;
    }
    let n = b.len().div_ceil(CHUNK);
    for (i, chunk) in b.chunks(CHUNK).enumerate() {
        let more = u8::from(i + 1 < n);
        out.extend_from_slice(b"\x1b_G");
        if i == 0 {
            out.extend_from_slice(control.as_bytes());
            out.extend_from_slice(format!(",m={more}").as_bytes());
        } else if quiet > 0 {
            out.extend_from_slice(format!("m={more},q={quiet}").as_bytes());
        } else {
            out.extend_from_slice(format!("m={more}").as_bytes());
        }
        out.push(b';');
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Direct transmission (`t=d`). `data` is raw pixels (or PNG bytes); compressed here when
/// `h.zlib` is set.
pub fn transmit_direct(out: &mut Vec<u8>, h: &Header, data: &[u8]) {
    if h.zlib {
        let z = zlib(data, 1);
        write_chunked(out, &h.control('d', None), &z, h.quiet);
    } else {
        write_chunked(out, &h.control('d', None), data, h.quiet);
    }
}

/// Shared-memory transmission (`t=s`): the terminal maps `name`, reads `size` bytes and unlinks
/// it. The caller writes the object with [`shm::write`] first.
pub fn transmit_shm(out: &mut Vec<u8>, h: &Header, name: &str, size: usize) {
    write_chunked(out, &h.control('s', Some(size)), name.as_bytes(), h.quiet);
}

/// Temp-file transmission (`t=t`): the terminal reads and deletes the file. Kitty only accepts
/// paths in a temp directory containing `tty-graphics-protocol`.
pub fn transmit_temp_file(out: &mut Vec<u8>, h: &Header, path: &Path, size: usize) {
    write_chunked(
        out,
        &h.control('t', Some(size)),
        path.as_os_str().as_encoded_bytes(),
        h.quiet,
    );
}

/// Delete an image and free its data (`a=d,d=I`).
pub fn delete_image(out: &mut Vec<u8>, id: u32) {
    out.extend_from_slice(format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\").as_bytes());
}

/// SGR selecting the foreground colour that encodes `id`'s low 24 bits (8-bit colour for ids
/// below 256, as the spec allows; 24-bit otherwise).
pub fn id_color_sgr(id: u32) -> String {
    let low = id & 0x00FF_FFFF;
    if id < 256 {
        format!("\x1b[38;5;{low}m")
    } else {
        format!(
            "\x1b[38;2;{};{};{}m",
            (low >> 16) & 0xFF,
            (low >> 8) & 0xFF,
            low & 0xFF
        )
    }
}

/// One placeholder row of a virtual placement: `cols` cells for placement row `row`, starting
/// at column `col0`. With `compact`, only the first cell carries diacritics; the rest inherit
/// row (and MSB) from the left neighbour with column + 1, per the spec. Includes the colour SGR
/// and resets the foreground afterwards; the caller positions the cursor.
pub fn placeholder_row(id: u32, row: u16, col0: u16, cols: u16, compact: bool) -> Result<String> {
    if row as usize >= DIACRITICS.len() || (col0 + cols) as usize > DIACRITICS.len() {
        bail!("placement too large for placeholder diacritics (max 297 rows/cols)");
    }
    let msb = (id >> 24) as usize;
    let mut s = id_color_sgr(id);
    for c in 0..cols {
        s.push(PLACEHOLDER);
        if c == 0 || !compact {
            s.push(DIACRITICS[row as usize]);
            s.push(DIACRITICS[(col0 + c) as usize]);
            if msb != 0 {
                s.push(DIACRITICS[msb]);
            }
        }
    }
    s.push_str("\x1b[39m");
    Ok(s)
}

/// Decode a placeholder cell's diacritics back to `(row, col, msb)` (for tests and inbound use).
pub fn decode_placeholder(cell: &str) -> Option<(Option<u16>, Option<u16>, Option<u8>)> {
    let mut it = cell.chars();
    if it.next()? != PLACEHOLDER {
        return None;
    }
    let idx = |c: char| DIACRITICS.iter().position(|&d| d == c);
    let row = it.next().and_then(idx).map(|v| v as u16);
    let col = it.next().and_then(idx).map(|v| v as u16);
    let msb = it.next().and_then(idx).map(|v| v as u8);
    Some((row, col, msb))
}

/// Process-unique names for shm objects / temp files.
pub fn unique_name(prefix: &str) -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    format!(
        "{prefix}{:x}-{:x}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    )
}

/// POSIX shared memory objects for `t=s`.
pub mod shm {
    use super::*;
    use std::ffi::CString;

    /// macOS limits shm names to 31 bytes (`PSHMNAMLEN`), including the leading `/`.
    pub const MAX_NAME: usize = 31;

    pub fn new_name() -> String {
        unique_name("/vkb-")
    }

    /// Create `name` (exclusive, mode 0600) holding exactly `data`.
    pub fn write(name: &str, data: &[u8]) -> Result<()> {
        if name.len() > MAX_NAME || !name.starts_with('/') {
            bail!("bad shm name {name:?}");
        }
        let c = CString::new(name)?;
        // SAFETY: c is a valid C string; flags/mode are plain ints (mode passed as c_uint for
        // the variadic macOS signature).
        let fd = unsafe {
            libc::shm_open(
                c.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_RDWR,
                0o600 as libc::c_uint,
            )
        };
        if fd < 0 {
            bail!("shm_open {name}: {}", std::io::Error::last_os_error());
        }
        let res = (|| -> Result<()> {
            // SAFETY: fd is a valid shm descriptor.
            if unsafe { libc::ftruncate(fd, data.len() as libc::off_t) } != 0 {
                bail!("ftruncate: {}", std::io::Error::last_os_error());
            }
            if data.is_empty() {
                return Ok(());
            }
            // SAFETY: mapping a freshly sized object we own, writable and shared.
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    data.len(),
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                bail!("mmap: {}", std::io::Error::last_os_error());
            }
            // SAFETY: p points to data.len() writable bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(data.as_ptr(), p.cast::<u8>(), data.len());
                libc::munmap(p, data.len());
            }
            Ok(())
        })();
        // SAFETY: closing our descriptor; the object stays until unlinked.
        unsafe { libc::close(fd) };
        if res.is_err() {
            unlink(name);
        }
        res
    }

    /// Read `len` bytes back (what the terminal does), for tests and the bench.
    pub fn read(name: &str, len: usize) -> Result<Vec<u8>> {
        let c = CString::new(name)?;
        // SAFETY: valid C string; read-only open.
        let fd = unsafe { libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0 as libc::c_uint) };
        if fd < 0 {
            bail!("shm_open {name}: {}", std::io::Error::last_os_error());
        }
        let mut out = vec![0u8; len];
        if len > 0 {
            // SAFETY: mapping len bytes of an object at least that large, read-only.
            let p = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    len,
                    libc::PROT_READ,
                    libc::MAP_SHARED,
                    fd,
                    0,
                )
            };
            if p == libc::MAP_FAILED {
                // SAFETY: closing our fd.
                unsafe { libc::close(fd) };
                bail!("mmap: {}", std::io::Error::last_os_error());
            }
            // SAFETY: p points to len readable bytes.
            unsafe {
                std::ptr::copy_nonoverlapping(p.cast::<u8>(), out.as_mut_ptr(), len);
                libc::munmap(p, len);
            }
        }
        // SAFETY: closing our fd.
        unsafe { libc::close(fd) };
        Ok(out)
    }

    pub fn unlink(name: &str) {
        if let Ok(c) = CString::new(name) {
            // SAFETY: valid C string.
            unsafe { libc::shm_unlink(c.as_ptr()) };
        }
    }
}

/// Write `data` to a fresh temp file whose name kitty accepts for `t=t`.
pub fn write_temp_file(dir: &Path, data: &[u8]) -> Result<PathBuf> {
    let p = dir.join(format!("{}.rgba", unique_name("tty-graphics-protocol-vk-")));
    std::fs::write(&p, data).with_context(|| format!("write {}", p.display()))?;
    Ok(p)
}

/// How tile pixels reach the host terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transfer {
    /// Base64 through the PTY; `zlib` deflates raw pixels first.
    Direct { format: PixelFormat, zlib: bool },
    /// POSIX shm per tile (`t=s`, raw RGBA); local hosts only.
    Shm,
    /// Temp file per tile (`t=t`, raw RGBA) in `dir`; local hosts only.
    TempFile { dir: PathBuf },
}

/// Bytes and side-channel cost of one encoded update.
#[derive(Debug, Default, Clone, Copy)]
pub struct EncodeStats {
    /// Bytes written to the PTY (escape sequences).
    pub pty_bytes: usize,
    /// Bytes moved out of band (shm or temp files).
    pub side_bytes: usize,
    pub tiles: usize,
}

/// Encodes changed tiles of a frame as kitty commands for one browser pane.
#[derive(Debug, Clone)]
pub struct TileEncoder {
    pub transfer: Transfer,
    /// Image id of tile 0; tile `i` uses `base_id + i`.
    pub base_id: u32,
    pub cell_w: u32,
    pub cell_h: u32,
    /// `q=` for every command (2 = no replies; 0 lets tests see each `OK`).
    pub quiet: u8,
    /// Created shm objects / temp files not yet consumed (the bench cleans them up; a real
    /// host unlinks them after reading).
    pub leftovers: Vec<String>,
}

impl TileEncoder {
    pub fn new(transfer: Transfer, base_id: u32, cell_w: u32, cell_h: u32) -> TileEncoder {
        TileEncoder {
            transfer,
            base_id,
            cell_w,
            cell_h,
            quiet: 2,
            leftovers: Vec::new(),
        }
    }

    /// Append commands for `tiles` of `img` to `out`.
    pub fn encode(
        &mut self,
        img: &Rgba,
        tiles: &[TileRect],
        out: &mut Vec<u8>,
    ) -> Result<EncodeStats> {
        let mut st = EncodeStats::default();
        let before = out.len();
        for t in tiles {
            let cells = (
                t.w.div_ceil(self.cell_w) as u16,
                t.h.div_ceil(self.cell_h) as u16,
            );
            let id = self.base_id + t.index as u32;
            let header = |format, w, h| {
                let mut hd = Header::new(id, format, w, h);
                hd.virtual_cells = Some(cells);
                hd.quiet = self.quiet;
                hd
            };
            match &self.transfer {
                Transfer::Direct { format, zlib } => {
                    let mut h = header(*format, t.w, t.h);
                    match format {
                        PixelFormat::Png => {
                            let png = crate::frame::encode_png(t.w, t.h, &img.extract(t), true)?;
                            transmit_direct(out, &h, &png);
                        }
                        PixelFormat::Rgb => {
                            h.zlib = *zlib;
                            transmit_direct(out, &h, &img.extract_rgb(t));
                        }
                        PixelFormat::Rgba => {
                            h.zlib = *zlib;
                            transmit_direct(out, &h, &img.extract(t));
                        }
                    }
                }
                Transfer::Shm => {
                    let px = img.extract(t);
                    let name = shm::new_name();
                    shm::write(&name, &px)?;
                    let h = header(PixelFormat::Rgba, t.w, t.h);
                    transmit_shm(out, &h, &name, px.len());
                    st.side_bytes += px.len();
                    self.leftovers.push(name);
                }
                Transfer::TempFile { dir } => {
                    let px = img.extract(t);
                    let path = write_temp_file(dir, &px)?;
                    let h = header(PixelFormat::Rgba, t.w, t.h);
                    transmit_temp_file(out, &h, &path, px.len());
                    st.side_bytes += px.len();
                    self.leftovers.push(path.display().to_string());
                }
            }
            st.tiles += 1;
        }
        st.pty_bytes = out.len() - before;
        Ok(st)
    }

    /// Remove shm objects / temp files the (absent) terminal never consumed.
    pub fn cleanup(&mut self) {
        for l in self.leftovers.drain(..) {
            if l.starts_with('/') && !l.contains("tty-graphics-protocol") {
                shm::unlink(&l);
            } else {
                let _ = std::fs::remove_file(&l);
            }
        }
    }

    /// Placeholder text for the whole pane: one line per cell row, each tile's cells carrying
    /// that tile's image id and its row/column inside the tile. Lines have no cursor movement;
    /// the compositor positions each line.
    pub fn placeholder_lines(
        &self,
        width: u32,
        height: u32,
        tile_w: u32,
        tile_h: u32,
    ) -> Result<Vec<String>> {
        let tile_cols = tile_w / self.cell_w;
        let tile_rows = tile_h / self.cell_h;
        let grid_cols = width.div_ceil(tile_w);
        let cell_cols = width.div_ceil(self.cell_w);
        let cell_rows = height.div_ceil(self.cell_h);
        let mut lines = Vec::with_capacity(cell_rows as usize);
        for cy in 0..cell_rows {
            let mut line = String::new();
            let trow = cy / tile_rows;
            let mut cx = 0;
            while cx < cell_cols {
                let tcol = cx / tile_cols;
                let id = self.base_id + trow * grid_cols + tcol;
                let n = tile_cols.min(cell_cols - cx);
                line.push_str(&placeholder_row(
                    id,
                    (cy % tile_rows) as u16,
                    0,
                    n as u16,
                    true,
                )?);
                cx += n;
            }
            lines.push(line);
        }
        Ok(lines)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::TileDiffer;

    fn split_commands(out: &[u8]) -> Vec<String> {
        let s = String::from_utf8(out.to_vec()).unwrap();
        s.split("\x1b\\")
            .filter(|x| !x.is_empty())
            .map(|x| x.strip_prefix("\x1b_G").expect("APC G").to_owned())
            .collect()
    }

    #[test]
    fn diacritics_table() {
        assert_eq!(DIACRITICS[0], '\u{0305}');
        assert_eq!(DIACRITICS[1], '\u{030D}');
        assert_eq!(DIACRITICS[296], '\u{1D244}');
        let mut sorted = DIACRITICS.to_vec();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 297);
    }

    #[test]
    fn small_payload_is_one_command() {
        let mut out = Vec::new();
        let h = Header::new(7, PixelFormat::Rgba, 1, 1);
        transmit_direct(&mut out, &h, &[1, 2, 3, 4]);
        let c = split_commands(&out);
        assert_eq!(c, vec!["a=T,i=7,f=32,t=d,q=2,s=1,v=1;AQIDBA==".to_owned()]);
    }

    #[test]
    fn chunking_follows_spec() {
        let data: Vec<u8> = (0..10_000u32).map(|i| (i * 7) as u8).collect();
        let mut out = Vec::new();
        let mut h = Header::new(42, PixelFormat::Rgba, 50, 50);
        h.virtual_cells = Some((4, 2));
        transmit_direct(&mut out, &h, &data);
        let cmds = split_commands(&out);
        // 10000 bytes → 13336 base64 bytes → 4 chunks.
        assert_eq!(cmds.len(), 4);
        let (ctrl0, p0) = cmds[0].split_once(';').unwrap();
        assert_eq!(ctrl0, "a=T,i=42,f=32,t=d,q=2,s=50,v=50,U=1,c=4,r=2,m=1");
        assert_eq!(p0.len(), CHUNK);
        assert!(cmds[1].starts_with("m=1,q=2;"));
        assert!(cmds[2].starts_with("m=1,q=2;"));
        assert!(cmds[3].starts_with("m=0,q=2;"));
        let mut b64 = String::new();
        for c in &cmds {
            let (_, p) = c.split_once(';').unwrap();
            assert!(p.len() <= CHUNK);
            b64.push_str(p);
        }
        for c in &cmds[..3] {
            assert_eq!(c.split_once(';').unwrap().1.len() % 4, 0);
        }
        let back = base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn zlib_payload_roundtrips() {
        use std::io::Read;
        let data = vec![200u8; 64 * 64 * 4];
        let mut out = Vec::new();
        let mut h = Header::new(3, PixelFormat::Rgba, 64, 64);
        h.zlib = true;
        transmit_direct(&mut out, &h, &data);
        let cmds = split_commands(&out);
        assert_eq!(cmds.len(), 1, "a flat tile deflates to one chunk");
        let (ctrl, p) = cmds[0].split_once(';').unwrap();
        assert!(ctrl.contains(",o=z"));
        let z = base64::engine::general_purpose::STANDARD.decode(p).unwrap();
        let mut back = Vec::new();
        flate2::read::ZlibDecoder::new(&z[..])
            .read_to_end(&mut back)
            .unwrap();
        assert_eq!(back, data);
    }

    #[test]
    fn shm_and_file_commands() {
        let mut out = Vec::new();
        let h = Header::new(9, PixelFormat::Rgba, 2, 2);
        transmit_shm(&mut out, &h, "/vkb-1", 16);
        let c = split_commands(&out);
        let (ctrl, p) = c[0].split_once(';').unwrap();
        assert_eq!(ctrl, "a=T,i=9,f=32,t=s,q=2,s=2,v=2,S=16");
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(p).unwrap(),
            b"/vkb-1"
        );
        let mut out = Vec::new();
        transmit_temp_file(&mut out, &h, Path::new("/tmp/tty-graphics-protocol-x"), 16);
        assert!(split_commands(&out)[0].starts_with("a=T,i=9,f=32,t=t,q=2,s=2,v=2,S=16;"));
        let mut out = Vec::new();
        delete_image(&mut out, 9);
        assert_eq!(out, b"\x1b_Ga=d,d=I,i=9,q=2\x1b\\");
    }

    #[test]
    fn shm_roundtrip() {
        let name = shm::new_name();
        assert!(name.len() <= shm::MAX_NAME);
        let data: Vec<u8> = (0..20_000u32).map(|i| i as u8).collect();
        shm::write(&name, &data).unwrap();
        assert!(shm::write(&name, &data).is_err(), "exclusive create");
        assert_eq!(shm::read(&name, data.len()).unwrap(), data);
        shm::unlink(&name);
        assert!(shm::read(&name, 1).is_err());
    }

    #[test]
    fn placeholder_encoding() {
        // Small id: 8-bit colour, full diacritics on every cell.
        let s = placeholder_row(5, 1, 0, 3, false).unwrap();
        assert!(s.starts_with("\x1b[38;5;5m"));
        assert!(s.ends_with("\x1b[39m"));
        let body = s
            .trim_start_matches("\x1b[38;5;5m")
            .trim_end_matches("\x1b[39m");
        let cells: Vec<String> = body
            .split(PLACEHOLDER)
            .skip(1)
            .map(|c| format!("{PLACEHOLDER}{c}"))
            .collect();
        assert_eq!(cells.len(), 3);
        for (i, c) in cells.iter().enumerate() {
            assert_eq!(decode_placeholder(c), Some((Some(1), Some(i as u16), None)));
        }
        // Compact: only the first cell has diacritics.
        let s = placeholder_row(5, 2, 4, 3, true).unwrap();
        assert_eq!(s.chars().filter(|&c| c == PLACEHOLDER).count(), 3);
        assert_eq!(
            s.chars().filter(|c| DIACRITICS.contains(c)).count(),
            2,
            "row + col on the first cell only"
        );
        // Large id: 24-bit colour + MSB diacritic.
        let id = 0x0312_3456;
        let s = placeholder_row(id, 0, 0, 1, true).unwrap();
        assert!(s.starts_with("\x1b[38;2;18;52;86m"));
        let cell: String = s
            .chars()
            .skip_while(|&c| c != PLACEHOLDER)
            .take(4)
            .collect();
        assert_eq!(decode_placeholder(&cell), Some((Some(0), Some(0), Some(3))));
        assert!(placeholder_row(1, 297, 0, 1, true).is_err());
        assert!(placeholder_row(1, 0, 290, 8, true).is_err());
    }

    #[test]
    fn tile_encoder_direct_shm_file() {
        let mut img = Rgba::new(128, 64);
        img.fill_rect(0, 0, 128, 64, [255, 255, 255, 255]);
        let mut d = TileDiffer::cell_aligned(16, 32, 4, 2);
        let tiles = d.diff(&img);
        assert_eq!(tiles.len(), 2);
        for transfer in [
            Transfer::Direct {
                format: PixelFormat::Rgba,
                zlib: true,
            },
            Transfer::Direct {
                format: PixelFormat::Rgb,
                zlib: false,
            },
            Transfer::Direct {
                format: PixelFormat::Png,
                zlib: false,
            },
            Transfer::Shm,
            Transfer::TempFile {
                dir: std::env::temp_dir(),
            },
        ] {
            let mut e = TileEncoder::new(transfer.clone(), 1000, 16, 32);
            let mut out = Vec::new();
            let st = e.encode(&img, &tiles, &mut out).unwrap();
            assert_eq!(st.tiles, 2);
            assert_eq!(st.pty_bytes, out.len());
            let cmds = split_commands(&out);
            assert!(cmds[0].contains("i=1000,"), "{transfer:?}: {}", cmds[0]);
            assert!(cmds.iter().any(|c| c.contains("i=1001,")));
            assert!(cmds[0].contains("U=1,c=4,r=2"));
            if matches!(transfer, Transfer::Shm) {
                assert_eq!(st.side_bytes, 2 * 64 * 64 * 4);
                let name = e.leftovers[0].clone();
                assert_eq!(
                    shm::read(&name, 64 * 64 * 4).unwrap(),
                    img.extract(&tiles[0])
                );
            }
            e.cleanup();
            assert!(e.leftovers.is_empty());
        }
    }

    #[test]
    fn placeholder_lines_cover_pane() {
        let e = TileEncoder::new(Transfer::Shm, 100, 16, 32);
        // 10 cols × 3 rows of 16×32 cells, tiles of 4×2 cells.
        let lines = e.placeholder_lines(160, 96, 64, 64).unwrap();
        assert_eq!(lines.len(), 3);
        for l in &lines {
            assert_eq!(l.chars().filter(|&c| c == PLACEHOLDER).count(), 10);
        }
        // Row 0: tiles 100,101,102 (last tile 2 cells wide). Row 2: second tile row 103..105.
        assert!(lines[0].contains("\x1b[38;5;100m"));
        assert!(lines[0].contains("\x1b[38;5;102m"));
        assert!(lines[2].contains("\x1b[38;5;103m"));
        assert!(lines[2].contains("\x1b[38;5;105m"));
    }
}
