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
const FLUSH_BATCH_BYTES: usize = 8 << 20;

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
    generation: u64,
    flush_revision: u64,
    path: PathBuf,
    written: usize,
    buf: Vec<u8>,
    /// Sealed segment: each flush appends one encrypted record.
    cipher: Option<Arc<StateCipher>>,
}

pub struct Archive {
    root: PathBuf,
    open: HashMap<String, PaneSeg>,
    /// Rotated segments stay readable until housekeeping writes their remaining rows.
    pending: HashMap<String, Vec<PaneSeg>>,
    next_generation: u64,
    cipher: Option<Arc<StateCipher>>,
    /// Encryption was asked for but the key is locked: nothing is written (fail closed).
    paused: bool,
}

/// An immutable archive prefix that can be compressed outside the archive lock.
pub struct FlushBatch {
    pane: String,
    generation: u64,
    flush_revision: u64,
    bytes: Vec<u8>,
    cipher: Option<Arc<StateCipher>>,
}

pub struct CompressedFlush {
    pane: String,
    generation: u64,
    flush_revision: u64,
    len: usize,
    frame: Vec<u8>,
}

impl FlushBatch {
    pub fn bytes_len(&self) -> usize {
        self.bytes.len()
    }

    pub fn compress(self) -> Result<CompressedFlush> {
        let mut frame = zstd::encode_all(&self.bytes[..], 3)?;
        if let Some(c) = self.cipher {
            frame = c.seal(&frame);
        }
        Ok(CompressedFlush {
            pane: self.pane,
            generation: self.generation,
            flush_revision: self.flush_revision,
            len: self.bytes.len(),
            frame,
        })
    }
}

impl Archive {
    pub fn new(root: &Path) -> Self {
        Archive {
            root: root.to_path_buf(),
            open: HashMap::new(),
            pending: HashMap::new(),
            next_generation: 0,
            cipher: None,
            paused: false,
        }
    }

    /// Stop writing (encryption requested, key locked) or resume. Pausing flushes what was
    /// buffered before and closes every open segment; rows appended while paused are dropped.
    pub fn set_paused(&mut self, paused: bool) -> Result<()> {
        if paused && !self.paused {
            self.flush()?;
            self.open.clear();
        }
        self.paused = paused;
        Ok(())
    }

    /// Writes are paused (see [`Archive::set_paused`]).
    pub fn paused(&self) -> bool {
        self.paused
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
        if rows.is_empty() || self.paused {
            return Ok(());
        }
        let dir = self.pane_dir(pane);
        let seg = match self.open.get_mut(pane) {
            Some(s) if s.written < SEGMENT_BYTES => s,
            _ => {
                if let Some(old) = self.open.remove(pane) {
                    self.pending.entry(pane.to_string()).or_default().push(old);
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
                self.next_generation += 1;
                self.open.entry(pane.to_string()).or_insert(PaneSeg {
                    generation: self.next_generation,
                    flush_revision: 0,
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
        for segs in self.pending.values_mut() {
            for s in segs {
                flush_seg(s)?;
            }
        }
        self.pending.clear();
        for s in self.open.values_mut() {
            flush_seg(s)?;
        }
        Ok(())
    }

    pub fn close_pane(&mut self, pane: &str) -> Result<()> {
        if let Some(segs) = self.pending.get_mut(pane) {
            for s in segs {
                flush_seg(s)?;
            }
        }
        self.pending.remove(pane);
        // Retain the buffer if writing fails, so a later flush can retry.
        if let Some(s) = self.open.get_mut(pane) {
            flush_seg(s)?;
        }
        self.open.remove(pane);
        Ok(())
    }

    pub fn remove_pane(&mut self, pane: &str) {
        self.open.remove(pane);
        self.pending.remove(pane);
        let _ = std::fs::remove_dir_all(self.pane_dir(pane));
    }

    /// Highest archived absolute line number for a pane (to resume after restart).
    pub fn last_line(&mut self, pane: &str) -> Result<Option<u64>> {
        let segs = self.readable_segments(pane);
        let Some(last) = segs.last() else {
            return Ok(None);
        };
        Ok(self.read_buffered_segment(pane, last)?.last().map(|r| r.n))
    }

    /// Lowest archived absolute line number still available (including pending writes).
    pub fn first_line(&mut self, pane: &str) -> Result<Option<u64>> {
        let segs = self.readable_segments(pane);
        let Some(first) = segs.first() else {
            return Ok(None);
        };
        Ok(self
            .read_buffered_segment(pane, first)?
            .first()
            .map(|r| r.n))
    }

    fn buffered_segments(&self, pane: &str) -> impl Iterator<Item = &PaneSeg> {
        self.pending
            .get(pane)
            .into_iter()
            .flatten()
            .chain(self.open.get(pane))
    }

    fn readable_segments(&self, pane: &str) -> Vec<PathBuf> {
        let mut paths = self.segments(pane);
        paths.extend(self.buffered_segments(pane).map(|s| s.path.clone()));
        paths.sort();
        paths.dedup();
        paths
    }

    fn read_buffered_segment(&self, pane: &str, path: &Path) -> Result<Vec<ArchivedRow>> {
        let mut rows = if path.exists() {
            read_seg(path)?
        } else {
            Vec::new()
        };
        for s in self.buffered_segments(pane).filter(|s| s.path == path) {
            rows.extend(parse_rows(&s.buf));
        }
        Ok(rows)
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
        let segs = self.readable_segments(pane);
        let starts: Vec<u64> = segs.iter().map(|p| seg_start(p)).collect();
        let mut out = Vec::new();
        for (i, p) in segs.iter().enumerate() {
            let next = starts.get(i + 1).copied().unwrap_or(u64::MAX);
            if next <= from || starts[i] >= to {
                continue;
            }
            out.extend(
                self.read_buffered_segment(pane, p)?
                    .into_iter()
                    .filter(|r| r.n >= from && r.n < to),
            );
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
        v.extend(self.pending.keys().cloned());
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
        self.pending.remove(pane);
    }

    /// Copy a stable prefix of every buffered segment. Compression can run after releasing
    /// the archive mutex. The original buffers stay readable until [`Self::apply_flush`].
    pub fn prepare_flush(&self) -> Vec<FlushBatch> {
        let mut batches = Vec::new();
        let mut bytes = 0;
        // Drain closed segments first, bounding temporary copies to about 8 MiB per pass
        // (plus the last segment). The caller schedules another pass if buffers remain.
        for (pane, s) in self
            .pending
            .iter()
            .flat_map(|(p, segs)| segs.iter().map(move |s| (p, s)))
            .chain(self.open.iter())
        {
            if s.buf.is_empty() {
                continue;
            }
            bytes += s.buf.len();
            batches.push(FlushBatch {
                pane: pane.clone(),
                generation: s.generation,
                flush_revision: s.flush_revision,
                bytes: s.buf.clone(),
                cipher: s.cipher.clone(),
            });
            if bytes >= FLUSH_BATCH_BYTES {
                break;
            }
        }
        batches
    }

    /// Uncompressed bytes waiting to be written, without copying the buffers.
    pub fn buffered_bytes(&self) -> usize {
        self.open
            .values()
            .chain(self.pending.values().flatten())
            .map(|s| s.buf.len())
            .sum()
    }

    /// Earliest row still buffered for each pane. The caller must not publish later rows
    /// into its durable search index before this prefix has been written.
    pub fn buffered_starts(&self) -> HashMap<String, u64> {
        let mut starts = HashMap::new();
        for (pane, s) in self.open.iter().chain(
            self.pending
                .iter()
                .flat_map(|(p, segs)| segs.iter().map(move |s| (p, s))),
        ) {
            if let Some(row) = parse_rows(&s.buf).next() {
                let start = starts.entry(pane.clone()).or_insert(row.n);
                *start = (*start).min(row.n);
            }
        }
        starts
    }

    /// Write a prepared prefix only if it still belongs to the same segment and no flush,
    /// purge or cipher change has consumed it. Concurrent appends remain buffered.
    pub fn apply_flush(&mut self, batch: CompressedFlush) -> Result<()> {
        let segment = self
            .open
            .get_mut(&batch.pane)
            .into_iter()
            .chain(self.pending.get_mut(&batch.pane).into_iter().flatten())
            .find(|s| s.generation == batch.generation);
        if let Some(s) = segment
            && s.flush_revision == batch.flush_revision
        {
            write_frame(s, &batch.frame)?;
            s.buf.drain(..batch.len);
            s.flush_revision += 1;
        }
        self.pending.retain(|_, segs| {
            segs.retain(|s| !s.buf.is_empty());
            !segs.is_empty()
        });
        Ok(())
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
    write_frame(s, &frame)?;
    s.buf.clear();
    s.flush_revision += 1;
    Ok(())
}

fn write_frame(s: &PaneSeg, frame: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&s.path, std::fs::Permissions::from_mode(0o600));
    }
    f.write_all(frame)?;
    Ok(())
}

fn read_seg(p: &Path) -> Result<Vec<ArchivedRow>> {
    let raw = crypt::read_file(p)?;
    let text = zstd::decode_all(&raw[..])?;
    Ok(parse_rows(&text).collect())
}

fn parse_rows(text: &[u8]) -> impl Iterator<Item = ArchivedRow> + '_ {
    text.split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_slice(l).ok())
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
    fn reads_see_rotated_buffers_without_flushing_any_pane() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::new(d.path());
        let first = rows(0, 2, &"x".repeat(SEGMENT_BYTES / 2));
        a.append("p1", &first).unwrap();
        a.append("p1", &rows(2, 3, "next")).unwrap();
        a.append("p2", &rows(0, 1, "other")).unwrap();
        assert!(
            a.segments("p1").is_empty(),
            "rotation must not write or compress"
        );
        assert_eq!(a.first_line("p1").unwrap(), Some(0));
        assert_eq!(a.last_line("p1").unwrap(), Some(4));
        assert_eq!(
            a.read("p1", 1, 4)
                .unwrap()
                .iter()
                .map(|r| r.n)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(a.segments("p1").is_empty());
        assert!(a.segments("p2").is_empty(), "reading p1 must not flush p2");
        a.flush().unwrap();
        assert_eq!(a.read("p1", 0, 5).unwrap().len(), 5);
        assert_eq!(a.segments("p1").len(), 2);
    }

    #[test]
    fn prepared_prefix_remains_readable_through_append_and_rotation() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::new(d.path());
        a.append("p1", &rows(0, 2, "first")).unwrap();
        let batch = a.prepare_flush().pop().unwrap().compress().unwrap();
        a.append("p1", &rows(2, 1, &"x".repeat(SEGMENT_BYTES)))
            .unwrap();
        a.append("p1", &rows(3, 2, "rotated")).unwrap();
        let before = a.read("p1", 0, 5).unwrap();
        a.apply_flush(batch).unwrap();
        assert_eq!(a.read("p1", 0, 5).unwrap(), before);
        a.flush().unwrap();
        assert_eq!(a.read("p1", 0, 5).unwrap(), before);
    }

    #[test]
    fn stale_prepared_flush_cannot_duplicate_or_resurrect_rows() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::new(d.path());
        a.append("p1", &rows(0, 2, "first")).unwrap();
        let old = a.prepare_flush().pop().unwrap().compress().unwrap();
        a.flush().unwrap();
        a.append("p1", &rows(2, 2, "later")).unwrap();
        a.apply_flush(old).unwrap();
        assert_eq!(a.read("p1", 0, 4).unwrap().len(), 4);
        let removed = a.prepare_flush().pop().unwrap().compress().unwrap();
        a.remove_pane("p1");
        // Even reusing the path after a delete must not accept a previous generation.
        a.append("p1", &rows(0, 1, "new")).unwrap();
        a.apply_flush(removed).unwrap();
        assert_eq!(a.read("p1", 0, 4).unwrap(), rows(0, 1, "new"));
    }

    #[test]
    fn prepared_flush_cannot_cross_an_encryption_change_or_pause() {
        let d = tempfile::tempdir().unwrap();
        let mut a = Archive::new(d.path());
        a.append("p1", &rows(0, 2, "plain")).unwrap();
        let plain = a.prepare_flush().pop().unwrap().compress().unwrap();
        let (cipher, _) = StateCipher::generate();
        a.set_cipher(Some(Arc::new(cipher))).unwrap();
        a.append("p1", &rows(2, 2, "secret")).unwrap();
        a.apply_flush(plain).unwrap();
        let sealed = a.prepare_flush().pop().unwrap().compress().unwrap();
        a.set_paused(true).unwrap();
        a.apply_flush(sealed).unwrap();
        a.append("p1", &rows(4, 1, "dropped")).unwrap();
        assert_eq!(a.read("p1", 0, 10).unwrap().len(), 4);
        let segs = a.segment_infos("p1");
        assert_eq!(segs.len(), 2);
        assert!(crypt::file_is_sealed(&segs[1].path));
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
