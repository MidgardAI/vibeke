//! Unlimited scrollback archive (03 §11.2): rows that enter a pane's history are appended to
//! zstd segment files `scrollback/<pane>/<first-line>.zst` (one JSON object per row, frames
//! appended on each flush), rotated at ~1 MiB uncompressed. Text is also indexed in FTS5 by
//! the caller.
//!
//! With `security.encrypt_state` (09 §9.1) the archive holds a cipher: a segment created while
//! it is set is sealed ([`crate::crypt`]), one record per flush. A segment keeps the mode it was
//! created with; readers handle both.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::crypt::{self, StateCipher};

const SEGMENT_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ArchivedRow {
    /// Absolute line number in the pane's history.
    pub n: u64,
    /// Row text (trailing blanks trimmed).
    pub t: String,
    /// Row continues on the next line (soft wrap).
    #[serde(default)]
    pub w: bool,
}

struct PaneSeg {
    path: PathBuf,
    written: usize,
    buf: Vec<u8>,
    /// Sealed segment: each flush appends one encrypted record.
    cipher: Option<Arc<StateCipher>>,
}

pub struct Archive {
    root: PathBuf,
    open: HashMap<String, PaneSeg>,
    cipher: Option<Arc<StateCipher>>,
}

impl Archive {
    pub fn new(root: &Path) -> Self {
        Archive {
            root: root.to_path_buf(),
            open: HashMap::new(),
            cipher: None,
        }
    }

    /// Seal segments created from now on (`None`: write plain). Open segments are flushed and
    /// closed so the next rows start a segment in the new mode.
    pub fn set_cipher(&mut self, cipher: Option<Arc<StateCipher>>) -> Result<()> {
        let same = match (&self.cipher, &cipher) {
            (None, None) => true,
            (Some(a), Some(b)) => a.id() == b.id(),
            _ => false,
        };
        if same {
            return Ok(());
        }
        self.flush()?;
        self.open.clear();
        if let Some(c) = &cipher {
            crypt::register(c.clone());
        }
        self.cipher = cipher;
        Ok(())
    }

    /// The cipher new segments are sealed with.
    pub fn cipher(&self) -> Option<&Arc<StateCipher>> {
        self.cipher.as_ref()
    }

    /// The `scrollback/` directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn pane_dir(&self, pane: &str) -> PathBuf {
        self.root.join(pane)
    }

    pub fn append(&mut self, pane: &str, rows: &[ArchivedRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let dir = self.pane_dir(pane);
        let seg = match self.open.get_mut(pane) {
            Some(s) if s.written < SEGMENT_BYTES => s,
            _ => {
                if let Some(mut old) = self.open.remove(pane) {
                    flush_seg(&mut old)?;
                }
                std::fs::create_dir_all(&dir)?;
                let path = dir.join(format!("{:016}.zst", rows[0].n));
                // An existing file (same first line after a restart) keeps its own mode.
                let cipher = if path.exists() {
                    if crypt::file_is_sealed(&path) {
                        match &self.cipher {
                            Some(c) => Some(c.clone()),
                            None => anyhow::bail!(
                                "segment {} is sealed and encryption is off",
                                path.display()
                            ),
                        }
                    } else {
                        None
                    }
                } else {
                    self.cipher.clone()
                };
                self.open.entry(pane.to_string()).or_insert(PaneSeg {
                    path,
                    written: 0,
                    buf: Vec::new(),
                    cipher,
                })
            }
        };
        for r in rows {
            let line = serde_json::to_string(r)?;
            seg.buf.extend_from_slice(line.as_bytes());
            seg.buf.push(b'\n');
            seg.written += line.len() + 1;
        }
        Ok(())
    }

    /// Write buffered rows (called at least every second by the server).
    pub fn flush(&mut self) -> Result<()> {
        for s in self.open.values_mut() {
            flush_seg(s)?;
        }
        Ok(())
    }

    pub fn close_pane(&mut self, pane: &str) -> Result<()> {
        if let Some(mut s) = self.open.remove(pane) {
            flush_seg(&mut s)?;
        }
        Ok(())
    }

    pub fn remove_pane(&mut self, pane: &str) {
        self.open.remove(pane);
        let _ = std::fs::remove_dir_all(self.pane_dir(pane));
    }

    /// Highest archived absolute line number for a pane (to resume after restart).
    pub fn last_line(&mut self, pane: &str) -> Result<Option<u64>> {
        self.flush()?;
        let segs = self.segments(pane);
        let Some(last) = segs.last() else {
            return Ok(None);
        };
        Ok(read_seg(last)?.last().map(|r| r.n))
    }

    /// Lowest archived absolute line number still on disk (retention deletes old segments).
    pub fn first_line(&mut self, pane: &str) -> Result<Option<u64>> {
        self.flush()?;
        let segs = self.segments(pane);
        let Some(first) = segs.first() else {
            return Ok(None);
        };
        Ok(read_seg(first)?.first().map(|r| r.n))
    }

    fn segments(&self, pane: &str) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(self.pane_dir(pane))
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|x| x == "zst"))
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Rows with absolute line numbers in `[from, to)`.
    pub fn read(&mut self, pane: &str, from: u64, to: u64) -> Result<Vec<ArchivedRow>> {
        self.flush()?;
        let segs = self.segments(pane);
        let starts: Vec<u64> = segs.iter().map(|p| seg_start(p)).collect();
        let mut out = Vec::new();
        for (i, p) in segs.iter().enumerate() {
            let next = starts.get(i + 1).copied().unwrap_or(u64::MAX);
            if next <= from || starts[i] >= to {
                continue;
            }
            out.extend(read_seg(p)?.into_iter().filter(|r| r.n >= from && r.n < to));
        }
        Ok(out)
    }

    /// Pane ids that have a segment directory on disk or an open segment. Dot directories
    /// (the purge staging area `.trash/`) are not panes.
    pub fn pane_ids(&self) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(&self.root)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().is_dir())
                    .filter_map(|e| e.file_name().into_string().ok())
                    .filter(|n| !n.starts_with('.'))
                    .collect()
            })
            .unwrap_or_default();
        v.extend(self.open.keys().cloned());
        v.sort();
        v.dedup();
        v
    }

    /// Is a segment of this pane currently open for writing?
    pub fn is_open(&self, pane: &str) -> bool {
        self.open.contains_key(pane)
    }

    /// Discard the pane's open (buffered) segment state; its file is removed by the caller.
    pub fn drop_open(&mut self, pane: &str) {
        self.open.remove(pane);
    }

    /// Segments of `pane` (oldest first) chosen by `sel`. Retention selections never include
    /// the segment currently being written; [`Select::All`] does.
    pub fn select(&self, pane: &str, sel: Select) -> Vec<SegInfo> {
        let segs = self.segments(pane);
        let starts: Vec<u64> = segs.iter().map(|p| seg_start(p)).collect();
        let infos: Vec<SegInfo> = segs
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let md = std::fs::metadata(p).ok();
                SegInfo {
                    path: p.clone(),
                    start: starts[i],
                    end: starts.get(i + 1).copied().unwrap_or(u64::MAX),
                    bytes: md.as_ref().map(|m| m.len()).unwrap_or(0),
                    mtime_ms: md
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0),
                }
            })
            .collect();
        let is_open = |s: &SegInfo| self.open.get(pane).is_some_and(|o| o.path == s.path);
        match sel {
            Select::All => infos,
            Select::OlderThan(ms) => infos
                .into_iter()
                .filter(|s| !is_open(s) && s.mtime_ms < ms)
                .collect(),
            Select::OverBytes(max) => {
                let mut total: u64 = infos.iter().map(|s| s.bytes).sum();
                let mut out = Vec::new();
                for s in infos {
                    if total <= max || is_open(&s) {
                        break;
                    }
                    total -= s.bytes;
                    out.push(s);
                }
                out
            }
        }
    }

    /// Every segment of `pane` on disk, oldest first.
    pub fn segment_infos(&self, pane: &str) -> Vec<SegInfo> {
        self.select(pane, Select::All)
    }

    /// Delete segment files (a missing file counts as deleted).
    pub fn remove_segments(segs: &[SegInfo]) -> Result<()> {
        for s in segs {
            match std::fs::remove_file(&s.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// Remove the pane's directory when it is empty.
    pub fn prune_dir(&self, pane: &str) {
        let _ = std::fs::remove_dir(self.pane_dir(pane));
    }
}

/// Which segments a purge takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Select {
    /// Everything, including the segment being written.
    All,
    /// Closed segments last written before this time (ms since epoch). A segment is the unit
    /// of deletion, so a segment that straddles the time stays whole.
    OlderThan(i64),
    /// Oldest closed segments until the pane is within this many compressed bytes.
    OverBytes(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegInfo {
    pub path: PathBuf,
    /// First absolute line of the segment (its file name).
    pub start: u64,
    /// Exclusive end: the next segment's start, `u64::MAX` for the last.
    pub end: u64,
    pub bytes: u64,
    pub mtime_ms: i64,
}

/// What reading one segment recovered.
#[derive(Debug)]
pub struct SegRead {
    pub rows: Vec<ArchivedRow>,
    /// The zstd stream or a row line was damaged; `rows` holds what was recoverable.
    pub damaged: bool,
}

/// Read a segment, salvaging the rows before a truncated or corrupt frame.
pub fn read_seg_lossy(p: &Path) -> std::io::Result<SegRead> {
    use std::io::Read;
    let raw = std::fs::read(p)?;
    let mut damaged = false;
    // A sealed segment: decrypt the good records (a missing key is an error, not damage).
    let raw = if crypt::is_sealed(&raw) {
        let o = crypt::open_lossy(&raw)?;
        damaged |= o.damaged;
        o.data
    } else {
        raw
    };
    let mut text = Vec::new();
    match zstd::stream::read::Decoder::new(&raw[..]) {
        Ok(mut d) => {
            let mut chunk = [0u8; 16 * 1024];
            loop {
                match d.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => text.extend_from_slice(&chunk[..n]),
                    Err(_) => {
                        damaged = true;
                        break;
                    }
                }
            }
        }
        Err(_) => damaged = true,
    }
    let mut rows = Vec::new();
    let lines = text.split(|&b| b == b'\n');
    for l in lines {
        if l.is_empty() {
            continue;
        }
        match serde_json::from_slice::<ArchivedRow>(l) {
            Ok(r) => rows.push(r),
            // A line cut short by a damaged stream is expected to be the last one.
            Err(_) => damaged = true,
        }
    }
    Ok(SegRead { rows, damaged })
}

fn seg_start(p: &Path) -> u64 {
    p.file_stem()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

fn flush_seg(s: &mut PaneSeg) -> Result<()> {
    if s.buf.is_empty() {
        return Ok(());
    }
    let mut frame = zstd::encode_all(&s.buf[..], 3)?;
    if let Some(c) = &s.cipher {
        frame = c.seal(&frame);
    }
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&s.path, std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(&frame)?;
    s.buf.clear();
    Ok(())
}

fn read_seg(p: &Path) -> Result<Vec<ArchivedRow>> {
    let raw = crypt::read_file(p)?;
    let text = zstd::decode_all(&raw[..])?;
    Ok(text
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_flush_read_rotate() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::new(d.path());
        for chunk in 0..30u64 {
            let rows: Vec<ArchivedRow> = (0..1000)
                .map(|i| ArchivedRow {
                    n: chunk * 1000 + i,
                    t: format!("line {} {}", chunk * 1000 + i, "x".repeat(40)),
                    w: false,
                })
                .collect();
            a.append("p1", &rows).unwrap();
            a.flush().unwrap();
        }
        assert!(a.segments("p1").len() > 1, "rotated");
        let r = a.read("p1", 15_500, 15_510).unwrap();
        assert_eq!(r.len(), 10);
        assert_eq!(r[0].n, 15_500);
        assert_eq!(a.last_line("p1").unwrap(), Some(29_999));
        assert_eq!(a.first_line("p1").unwrap(), Some(0));
        assert_eq!(a.first_line("nope").unwrap(), None);
        let sel = a.select("p1", Select::OverBytes(1));
        assert!(!sel.is_empty());
        Archive::remove_segments(&sel).unwrap();
        assert!(a.read("p1", 0, 10).unwrap().is_empty());
    }

    fn rows(from: u64, n: u64, word: &str) -> Vec<ArchivedRow> {
        (from..from + n)
            .map(|i| ArchivedRow {
                n: i,
                t: format!("{word} {i}"),
                w: false,
            })
            .collect()
    }

    #[test]
    fn sealed_segments_hide_text_and_read_back() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::new(d.path());
        // A plain segment first, then encryption turns on: the next rows start a sealed one.
        a.append("p1", &rows(0, 10, "plainword")).unwrap();
        a.flush().unwrap();
        let (c, _) = StateCipher::generate();
        a.set_cipher(Some(Arc::new(c))).unwrap();
        a.append("p1", &rows(10, 10, "SECRETWORD")).unwrap();
        a.flush().unwrap();
        a.append("p1", &rows(20, 5, "SECRETWORD")).unwrap();
        a.flush().unwrap();
        let segs = a.segment_infos("p1");
        assert_eq!(segs.len(), 2);
        assert!(!crypt::file_is_sealed(&segs[0].path));
        assert!(crypt::file_is_sealed(&segs[1].path));
        let raw = std::fs::read(&segs[1].path).unwrap();
        assert!(!raw.windows(10).any(|w| w == b"SECRETWORD"));
        // Both modes read back, in order, through every reader.
        let all = a.read("p1", 0, u64::MAX).unwrap();
        assert_eq!(all.len(), 25);
        assert_eq!(all[15].t, "SECRETWORD 15");
        let lossy = read_seg_lossy(&segs[1].path).unwrap();
        assert_eq!(lossy.rows.len(), 15);
        assert!(!lossy.damaged);
        assert_eq!(a.last_line("p1").unwrap(), Some(24));
        // A torn last record: the earlier flush survives a lossy read.
        let mut torn = raw.clone();
        torn.truncate(raw.len() - 5);
        std::fs::write(&segs[1].path, &torn).unwrap();
        let lossy = read_seg_lossy(&segs[1].path).unwrap();
        assert!(lossy.damaged);
        assert_eq!(lossy.rows.len(), 10);
    }
}
