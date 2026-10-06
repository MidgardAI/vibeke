//! Unlimited scrollback archive (03 §11.2): rows that enter a pane's history are appended to
//! zstd segment files `scrollback/<pane>/<first-line>.zst` (one JSON object per row, frames
//! appended on each flush), rotated at ~1 MiB uncompressed. Text is also indexed in FTS5 by
//! the caller.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

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
}

pub struct Archive {
    root: PathBuf,
    open: HashMap<String, PaneSeg>,
}

impl Archive {
    pub fn new(root: &Path) -> Self {
        Archive {
            root: root.to_path_buf(),
            open: HashMap::new(),
        }
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
                self.open.entry(pane.to_string()).or_insert(PaneSeg {
                    path,
                    written: 0,
                    buf: Vec::new(),
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

    /// Delete the oldest segments until the pane's archive is under `max_bytes` (compressed).
    pub fn enforce_retention(&mut self, pane: &str, max_bytes: u64) -> Result<()> {
        let segs = self.segments(pane);
        let sizes: Vec<u64> = segs
            .iter()
            .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
            .collect();
        let mut total: u64 = sizes.iter().sum();
        for (p, s) in segs.iter().zip(sizes) {
            if total <= max_bytes || self.open.get(pane).is_some_and(|o| &o.path == p) {
                break;
            }
            std::fs::remove_file(p)?;
            total -= s;
        }
        Ok(())
    }
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
    let frame = zstd::encode_all(&s.buf[..], 3)?;
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
    let raw = std::fs::read(p)?;
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
        a.enforce_retention("p1", 1).unwrap();
        assert!(a.read("p1", 0, 10).unwrap().is_empty());
    }
}
