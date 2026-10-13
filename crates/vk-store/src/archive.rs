//! Unlimited scrollback archive (03 §11.2): rows that enter a pane's history are appended to
//! zstd segment files `scrollback/<pane>/<first-line>.zst` (one JSON object per row, frames
//! appended on each flush), rotated at ~1 MiB uncompressed. Text is also indexed in FTS5 by
//! the caller.
//!
//! With `security.encrypt_state` (09 §9.1) the archive holds a cipher: a segment created while
//! it is set is sealed ([`crate::crypt`]), one record per flush. A segment keeps the mode it was
//! created with; readers handle both.

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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
    /// A failed append whose truncation must be retried before any later write.
    rollback_len: Option<u64>,
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
    read_fence: ReadFence,
    cipher: Option<Arc<StateCipher>>,
    /// Encryption was asked for but the key is locked: nothing is written (fail closed).
    paused: bool,
}

/// A lock-free invalidation fence for queued archive reads, including a caller's captured
/// memory rows. Clone it while constructing the server; reads never need the archive mutex.
#[derive(Clone, Default)]
pub struct ReadFence(Arc<AtomicU64>);

impl ReadFence {
    pub fn generation(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

/// Stable segment prefixes and pending bytes, captured while holding the archive mutex.
/// File handles keep the selected inodes alive across rename/replacement; their captured
/// lengths exclude later flushes, which would otherwise duplicate the copied buffer rows.
pub struct PreparedRead {
    segments: Vec<ReadSegment>,
    from: u64,
    to: u64,
    generation: u64,
    fence: ReadFence,
}

struct ReadSegment {
    file: Option<(std::fs::File, u64)>,
    buffered: Vec<u8>,
}

impl PreparedRead {
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Read and decode without holding the archive mutex. A purge invalidates the result.
    /// The caller must check its enqueue-time fence again immediately before publishing
    /// any result, including separately captured memory rows. This check is the result's
    /// publication point; work invalidated before it must not enter the outgoing queue.
    pub fn finish(self) -> Result<Vec<ArchivedRow>> {
        ensure!(
            self.fence.generation() == self.generation,
            "archive read invalidated"
        );
        let mut rows = Vec::new();
        for segment in self.segments {
            if let Some((file, len)) = segment.file
                && len > 0
            {
                let mut raw = Vec::new();
                file.take(len).read_to_end(&mut raw)?;
                ensure!(raw.len() as u64 == len, "archive prefix was truncated");
                let compressed = if crypt::is_sealed(&raw) {
                    crypt::open_all(&raw)?
                } else {
                    raw
                };
                let text = zstd::decode_all(&compressed[..])?;
                rows.extend(parse_rows(&text).filter(|r| r.n >= self.from && r.n < self.to));
            }
            rows.extend(
                parse_rows(&segment.buffered).filter(|r| r.n >= self.from && r.n < self.to),
            );
        }
        ensure!(
            self.fence.generation() == self.generation,
            "archive read invalidated"
        );
        Ok(rows)
    }
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
            read_fence: ReadFence::default(),
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

    /// Check recovery from an archive storage failure with an actual write and sync. The
    /// private probe contains no pane data and is removed on both success and failure.
    pub fn probe(&self) -> Result<()> {
        std::fs::create_dir_all(&self.root)?;
        let path = self.root.join(format!(".probe-{}", ulid::Ulid::generate()));
        let mut options = std::fs::OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        let result = file
            .write_all(b"archive probe\n")
            .and_then(|()| file.sync_data());
        drop(file);
        let removed = std::fs::remove_file(&path);
        result?;
        removed?;
        Ok(())
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
                    rollback_len: None,
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
        self.invalidate_reads();
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

    fn capture_segment(&self, pane: &str, path: &Path) -> Result<ReadSegment> {
        let mut buffered = Vec::new();
        let mut rollback_len: Option<u64> = None;
        for segment in self.buffered_segments(pane).filter(|s| s.path == path) {
            buffered.extend_from_slice(&segment.buf);
            if let Some(len) = segment.rollback_len {
                rollback_len = Some(rollback_len.map_or(len, |old| old.min(len)));
            }
        }
        let file = match std::fs::File::open(path) {
            Ok(file) => {
                let len = file.metadata()?.len();
                Some((file, rollback_len.map_or(len, |old| old.min(len))))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        Ok(ReadSegment { file, buffered })
    }

    fn read_buffered_segment(&self, pane: &str, path: &Path) -> Result<Vec<ArchivedRow>> {
        PreparedRead {
            segments: vec![self.capture_segment(pane, path)?],
            from: 0,
            to: u64::MAX,
            generation: self.read_generation(),
            fence: self.read_fence(),
        }
        .finish()
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

    /// Fence to retain for lock-free enqueue-time and publication-time checks.
    pub fn read_fence(&self) -> ReadFence {
        self.read_fence.clone()
    }

    pub fn read_generation(&self) -> u64 {
        self.read_fence.generation()
    }

    /// Invalidate outstanding reads before deleting, replacing or forgetting any history.
    /// Called under the archive mutex, including by retention/purge code which renames files
    /// itself. Conservative invalidation is safe if deletion later rolls back.
    pub fn invalidate_reads(&mut self) {
        self.read_fence.0.fetch_add(1, Ordering::Release);
    }

    /// Capture rows with absolute line numbers in `[from, to)`. The caller holds the archive
    /// mutex only for file opens, length snapshots and buffer copies, then releases it before
    /// calling [`PreparedRead::finish`]. Concurrent appends and flushes do not invalidate a
    /// read. Deletion and forgetting do; retain [`Self::read_fence`] for publication checks.
    pub fn prepare_read(&self, pane: &str, from: u64, to: u64) -> Result<PreparedRead> {
        let paths = self.readable_segments(pane);
        let starts: Vec<u64> = paths.iter().map(|p| seg_start(p)).collect();
        let mut segments = Vec::new();
        for (i, path) in paths.iter().enumerate() {
            let next = starts.get(i + 1).copied().unwrap_or(u64::MAX);
            if next <= from || starts[i] >= to {
                continue;
            }
            segments.push(self.capture_segment(pane, path)?);
        }
        Ok(PreparedRead {
            segments,
            from,
            to,
            generation: self.read_generation(),
            fence: self.read_fence(),
        })
    }

    /// Synchronous reader. Callers sharing an archive mutex should use [`Self::prepare_read`]
    /// and decode after releasing the lock.
    pub fn read(&mut self, pane: &str, from: u64, to: u64) -> Result<Vec<ArchivedRow>> {
        self.prepare_read(pane, from, to)?.finish()
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

    /// Does this pane have an open segment or a rotated segment still waiting to be written?
    pub fn is_open(&self, pane: &str) -> bool {
        self.open.contains_key(pane)
            || self
                .pending
                .get(pane)
                .is_some_and(|segs| segs.iter().any(|s| !s.buf.is_empty()))
    }

    /// Discard the pane's open (buffered) segment state; its file is removed by the caller.
    pub fn drop_open(&mut self, pane: &str) {
        self.invalidate_reads();
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

    /// Segments of `pane` (oldest first) chosen by `sel`. Retention selections exclude the
    /// current segment and rotated segments with buffered rows; [`Select::All`] includes them.
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
        let is_writing = |s: &SegInfo| {
            self.open.get(pane).is_some_and(|o| o.path == s.path)
                || self
                    .pending
                    .get(pane)
                    .is_some_and(|segs| segs.iter().any(|p| p.path == s.path && !p.buf.is_empty()))
        };
        match sel {
            Select::All => infos,
            Select::OlderThan(ms) => infos
                .into_iter()
                .filter(|s| !is_writing(s) && s.mtime_ms < ms)
                .collect(),
            Select::OverBytes(max) => {
                let mut total: u64 = infos.iter().map(|s| s.bytes).sum();
                let mut out = Vec::new();
                for s in infos {
                    if total <= max || is_writing(&s) {
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
    pub fn remove_segments(&mut self, segs: &[SegInfo]) -> Result<()> {
        if !segs.is_empty() {
            self.invalidate_reads();
        }
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

fn write_frame(s: &mut PaneSeg, frame: &[u8]) -> Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&s.path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&s.path, std::fs::Permissions::from_mode(0o600));
    }
    // Retry an earlier rollback before any new append. Until it succeeds readers see only
    // the durable prefix and the untouched buffer, never the torn frame on disk.
    if let Some(len) = s.rollback_len {
        truncate_frame(&f, len)?;
        s.rollback_len = None;
    }
    let before = f.metadata()?.len();
    if let Err(error) = write_frame_bytes(&mut f, frame) {
        s.rollback_len = Some(before);
        truncate_frame(&f, before)
            .with_context(|| format!("roll back archive append after {error}"))?;
        s.rollback_len = None;
        return Err(error.into());
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    static FAIL_WRITE_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static FAIL_TRUNCATE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn truncate_frame(file: &std::fs::File, len: u64) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_TRUNCATE.replace(false) {
        return Err(std::io::Error::other("injected archive rollback failure"));
    }
    file.set_len(len)
}

fn write_frame_bytes(file: &mut std::fs::File, frame: &[u8]) -> std::io::Result<()> {
    #[cfg(test)]
    if let Some(n) = FAIL_WRITE_AFTER.take() {
        file.write_all(&frame[..n.min(frame.len())])?;
        return Err(std::io::Error::other("injected partial archive write"));
    }
    file.write_all(frame)
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
        a.remove_segments(&sel).unwrap();
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
    fn partial_frame_write_rolls_back_before_retry_for_plain_and_sealed_segments() {
        for sealed in [false, true] {
            for prepared_flush in [false, true] {
                let directory = tempfile::tempdir().unwrap();
                let mut archive = Archive::new(directory.path());
                if sealed {
                    let (cipher, _) = StateCipher::generate();
                    archive.set_cipher(Some(Arc::new(cipher))).unwrap();
                }
                archive.append("p", &rows(0, 2, "durable")).unwrap();
                archive.flush().unwrap();
                let path = archive.segment_infos("p")[0].path.clone();
                let original = std::fs::read(&path).unwrap();
                archive.append("p", &rows(2, 2, "pending")).unwrap();
                FAIL_WRITE_AFTER.set(Some(7));
                let failed = if prepared_flush {
                    let batch = archive.prepare_flush().pop().unwrap().compress().unwrap();
                    archive.apply_flush(batch)
                } else {
                    archive.flush()
                };
                assert!(failed.is_err());
                assert_eq!(std::fs::read(&path).unwrap(), original);
                let mut expected = rows(0, 2, "durable");
                expected.extend(rows(2, 2, "pending"));
                assert_eq!(archive.read("p", 0, 4).unwrap(), expected);
                archive.flush().unwrap();
                assert_eq!(
                    Archive::new(directory.path()).read("p", 0, 4).unwrap(),
                    expected
                );
            }
        }
    }

    #[test]
    fn failed_rollback_fences_reads_and_retries_before_any_more_appends() {
        let directory = tempfile::tempdir().unwrap();
        let mut archive = Archive::new(directory.path());
        archive.append("p", &rows(0, 2, "durable")).unwrap();
        archive.flush().unwrap();
        let path = archive.segment_infos("p")[0].path.clone();
        let original_len = path.metadata().unwrap().len();
        archive.append("p", &rows(2, 2, "pending")).unwrap();
        FAIL_WRITE_AFTER.set(Some(7));
        FAIL_TRUNCATE.set(true);
        assert!(archive.flush().is_err());
        assert_eq!(archive.open["p"].rollback_len, Some(original_len));
        assert_eq!(path.metadata().unwrap().len(), original_len + 7);
        let mut expected = rows(0, 2, "durable");
        expected.extend(rows(2, 2, "pending"));
        assert_eq!(archive.read("p", 0, 4).unwrap(), expected);
        // Storage has not recovered. The retry must fail at truncation, before appending.
        FAIL_TRUNCATE.set(true);
        assert!(archive.flush().is_err());
        assert_eq!(path.metadata().unwrap().len(), original_len + 7);
        assert_eq!(archive.open["p"].rollback_len, Some(original_len));
        archive.flush().unwrap();
        assert_eq!(archive.open["p"].rollback_len, None);
        assert_eq!(
            Archive::new(directory.path()).read("p", 0, 4).unwrap(),
            expected
        );
    }

    #[test]
    fn prepared_read_freezes_file_prefix_and_buffer_across_flush_and_rotation() {
        for sealed in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let mut archive = Archive::new(directory.path());
            if sealed {
                let (cipher, _) = StateCipher::generate();
                archive.set_cipher(Some(Arc::new(cipher))).unwrap();
            }
            archive.append("p", &rows(0, 2, "first")).unwrap();
            archive.flush().unwrap();
            archive.append("p", &rows(2, 2, "buffered")).unwrap();
            let read = archive.prepare_read("p", 0, 10).unwrap();
            let generation = read.generation();
            archive
                .append("p", &rows(4, 1, &"x".repeat(SEGMENT_BYTES)))
                .unwrap();
            archive.append("p", &rows(5, 1, "rotated")).unwrap();
            archive.flush().unwrap();
            let mut expected = rows(0, 2, "first");
            expected.extend(rows(2, 2, "buffered"));
            assert_eq!(read.finish().unwrap(), expected);
            assert_eq!(archive.read_generation(), generation);
            assert_eq!(archive.read("p", 0, 10).unwrap().len(), 6);
        }
    }

    #[test]
    fn prepared_read_uses_captured_inode_and_does_not_reopen_a_replaced_path() {
        let directory = tempfile::tempdir().unwrap();
        let mut archive = Archive::new(directory.path());
        archive.append("p", &rows(0, 2, "original")).unwrap();
        archive.flush().unwrap();
        let read = archive.prepare_read("p", 0, 10).unwrap();
        let path = archive.segment_infos("p")[0].path.clone();
        // Simulate an atomic file replacement. Actual forget/retention also advances the
        // read fence, which is checked separately below; an inode snapshot alone is not it.
        std::fs::rename(&path, directory.path().join("saved")).unwrap();
        std::fs::write(&path, b"replacement must never be opened").unwrap();
        assert_eq!(read.finish().unwrap(), rows(0, 2, "original"));
    }

    #[test]
    fn prepared_read_finishes_without_archive_mutex_and_fence_reads_are_lock_free() {
        use std::sync::Mutex;
        let directory = tempfile::tempdir().unwrap();
        let mut archive = Archive::new(directory.path());
        archive.append("p", &rows(0, 2, "first")).unwrap();
        archive.flush().unwrap();
        let read = archive.prepare_read("p", 0, 10).unwrap();
        let fence = archive.read_fence();
        let generation = fence.generation();
        let archive = Mutex::new(archive);
        let guard = archive.lock().unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            assert_eq!(fence.generation(), generation);
            sent.send(read.finish()).unwrap();
        });
        // The reader and fence must complete even while another operation holds the lock.
        let completed = received.recv_timeout(std::time::Duration::from_secs(2));
        drop(guard);
        assert_eq!(completed.unwrap().unwrap(), rows(0, 2, "first"));
        worker.join().unwrap();
    }

    #[test]
    fn forget_and_removal_invalidate_both_prepared_and_queued_reads() {
        for operation in ["drop_open", "remove_pane", "remove_segments"] {
            let directory = tempfile::tempdir().unwrap();
            let mut archive = Archive::new(directory.path());
            archive.append("p", &rows(0, 2, "forgotten")).unwrap();
            archive.flush().unwrap();
            archive.append("p", &rows(2, 1, "buffered")).unwrap();
            let read = archive.prepare_read("p", 0, 10).unwrap();
            let fence = archive.read_fence();
            let queued_generation = fence.generation();
            match operation {
                "drop_open" => archive.drop_open("p"),
                "remove_pane" => archive.remove_pane("p"),
                _ => archive
                    .remove_segments(&archive.segment_infos("p"))
                    .unwrap(),
            }
            assert_ne!(fence.generation(), queued_generation, "{operation}");
            assert!(read.finish().is_err(), "{operation}");
        }
    }

    #[test]
    fn storage_probe_checks_writes_and_leaves_no_files() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("archive");
        let archive = Archive::new(&root);
        archive.probe().unwrap();
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
        std::fs::remove_dir(&root).unwrap();
        std::fs::write(&root, b"not a directory").unwrap();
        assert!(archive.probe().is_err());
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
