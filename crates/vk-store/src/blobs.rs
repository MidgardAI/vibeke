//! The unified content-addressed blob store (02 §1.1 "Blob"): `<state>/blobs/<h2>/<blake3>.<ext>`
//! with an optional `<blake3>.json` metadata sidecar. Screenshots, pane screenshots, uploaded
//! files (the pane inbox keeps its path for the agent but is ingested here), tool outputs and
//! large diffs all land in this one store, so `blob.get/stat/stats/gc` see every one of them.
//!
//! The store never decides what is referenced: callers pass the set of hashes they still need to
//! [`BlobStore::gc`], and only blobs whose sidecar names a collectable `source` are ever removed.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// What to do with an existing metadata sidecar when a blob is stored again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MetaMode {
    /// The new metadata replaces the old (the writer owns the blob: screenshots).
    Replace,
    /// An existing sidecar stays (ingesting a copy of something that is already stored).
    IfAbsent,
}

/// `source` values in a sidecar whose blobs [`BlobStore::gc`] may remove once unreferenced.
pub const COLLECTABLE_SOURCES: &[&str] = &["inbox", "payload"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BlobInfo {
    pub hash: String,
    pub size: u64,
    pub mtime_ms: i64,
    /// Extensions stored for this hash (usually one).
    pub exts: Vec<String>,
    pub meta: Option<Value>,
}

impl BlobInfo {
    pub fn source(&self) -> &str {
        self.meta
            .as_ref()
            .and_then(|m| m.get("source"))
            .and_then(Value::as_str)
            .unwrap_or("")
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub count: usize,
    pub bytes: u64,
    /// Count and bytes per sidecar `source` (`""` = no source recorded).
    pub by_source: BTreeMap<String, (usize, u64)>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GcReport {
    pub removed: usize,
    pub bytes: u64,
    pub kept_referenced: usize,
    pub kept_young: usize,
    pub kept_uncollectable: usize,
    pub hashes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct BlobStore {
    root: PathBuf,
}

pub fn valid_hash(h: &str) -> bool {
    h.len() == 64 && h.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

fn ext_ok(ext: &str) -> bool {
    !ext.is_empty() && ext.len() <= 16 && ext.bytes().all(|c| c.is_ascii_alphanumeric())
}

fn mtime_ms(md: &std::fs::Metadata) -> i64 {
    md.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl BlobStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        BlobStore { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_of(&self, hash: &str) -> PathBuf {
        self.root.join(&hash[..2])
    }

    pub fn path_of(&self, hash: &str, ext: &str) -> PathBuf {
        self.dir_of(hash).join(format!("{hash}.{ext}"))
    }

    fn ensure_dir(&self, hash: &str) -> std::io::Result<PathBuf> {
        let dir = self.dir_of(hash);
        std::fs::create_dir_all(&dir)?;
        for d in [&self.root, &dir] {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
        }
        Ok(dir)
    }

    fn write_meta(
        &self,
        dir: &Path,
        hash: &str,
        meta: &Value,
        mode: MetaMode,
    ) -> std::io::Result<()> {
        let mpath = dir.join(format!("{hash}.json"));
        if mode == MetaMode::IfAbsent && mpath.exists() {
            return Ok(());
        }
        if meta.is_null() {
            return Ok(());
        }
        std::fs::write(&mpath, serde_json::to_vec_pretty(meta).unwrap_or_default())?;
        let _ = std::fs::set_permissions(&mpath, std::fs::Permissions::from_mode(0o600));
        Ok(())
    }

    /// Store `data`; returns its blake3 hash and path. Idempotent (same bytes, same path).
    pub fn put(
        &self,
        data: &[u8],
        ext: &str,
        meta: &Value,
        mode: MetaMode,
    ) -> std::io::Result<(String, PathBuf)> {
        let ext = if ext_ok(ext) { ext } else { "bin" };
        let hash = blake3::hash(data).to_hex().to_string();
        let dir = self.ensure_dir(&hash)?;
        let path = dir.join(format!("{hash}.{ext}"));
        if !path.exists() {
            let tmp = dir.join(format!(".{hash}.tmp-{}", std::process::id()));
            std::fs::write(&tmp, data)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            std::fs::rename(&tmp, &path)?;
        }
        self.write_meta(&dir, &hash, meta, mode)?;
        Ok((hash, path))
    }

    /// Ingest an existing file without reading it all into memory: hash it, then copy it into
    /// the store (the source stays where it is and stays user-writable; the blob does not).
    pub fn put_file(
        &self,
        src: &Path,
        ext: &str,
        meta: &Value,
        mode: MetaMode,
    ) -> std::io::Result<(String, PathBuf)> {
        let ext = if ext_ok(ext) { ext } else { "bin" };
        let hash = hash_file(src)?;
        let dir = self.ensure_dir(&hash)?;
        let path = dir.join(format!("{hash}.{ext}"));
        if !path.exists() {
            let tmp = dir.join(format!(".{hash}.tmp-{}", std::process::id()));
            let _ = std::fs::remove_file(&tmp);
            std::fs::copy(src, &tmp)?;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
            // The source may have changed between hashing and copying: verify the copy.
            if hash_file(&tmp)? != hash {
                let _ = std::fs::remove_file(&tmp);
                return Err(std::io::Error::other("source changed while ingesting"));
            }
            std::fs::rename(&tmp, &path)?;
        }
        self.write_meta(&dir, &hash, meta, mode)?;
        Ok((hash, path))
    }

    /// Everything stored under `hash`.
    pub fn find(&self, hash: &str) -> Option<BlobInfo> {
        if !valid_hash(hash) {
            return None;
        }
        let dir = self.dir_of(hash);
        let mut exts = vec![];
        let mut size = 0;
        let mut mtime = 0;
        for e in std::fs::read_dir(&dir).ok()?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(ext) = name.strip_prefix(&format!("{hash}.")) else {
                continue;
            };
            if ext == "json" {
                continue;
            }
            if let Ok(md) = e.metadata() {
                size = size.max(md.len());
                mtime = mtime.max(mtime_ms(&md));
            }
            exts.push(ext.to_string());
        }
        if exts.is_empty() {
            return None;
        }
        exts.sort();
        let meta = std::fs::read(dir.join(format!("{hash}.json")))
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok());
        Some(BlobInfo {
            hash: hash.to_string(),
            size,
            mtime_ms: mtime,
            exts,
            meta,
        })
    }

    /// Every blob in the store, ordered by hash.
    pub fn list(&self) -> Vec<BlobInfo> {
        let mut hashes = std::collections::BTreeSet::new();
        let Ok(rd) = std::fs::read_dir(&self.root) else {
            return vec![];
        };
        for d in rd.flatten() {
            let Ok(files) = std::fs::read_dir(d.path()) else {
                continue;
            };
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().into_owned();
                if let Some((h, _)) = name.split_once('.')
                    && valid_hash(h)
                {
                    hashes.insert(h.to_string());
                }
            }
        }
        hashes.iter().filter_map(|h| self.find(h)).collect()
    }

    pub fn stats(&self) -> Stats {
        let mut s = Stats::default();
        for b in self.list() {
            s.count += 1;
            s.bytes += b.size;
            let e = s.by_source.entry(b.source().to_string()).or_default();
            e.0 += 1;
            e.1 += b.size;
        }
        s
    }

    /// Remove every file of `hash` (data and sidecar). Returns the bytes freed.
    pub fn remove(&self, hash: &str) -> u64 {
        if !valid_hash(hash) {
            return 0;
        }
        let dir = self.dir_of(hash);
        let mut freed = 0;
        if let Ok(rd) = std::fs::read_dir(&dir) {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(&format!("{hash}.")) {
                    freed += e.metadata().map(|m| m.len()).unwrap_or(0);
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&dir);
        freed
    }

    /// Remove collectable blobs (see [`COLLECTABLE_SOURCES`]) that nothing in `referenced` names
    /// and that are older than `max_age_ms` (by file mtime). `dry_run` only reports.
    pub fn gc(
        &self,
        referenced: &HashSet<String>,
        now_ms: i64,
        max_age_ms: i64,
        dry_run: bool,
    ) -> GcReport {
        let mut r = GcReport::default();
        for b in self.list() {
            if !COLLECTABLE_SOURCES.contains(&b.source()) {
                r.kept_uncollectable += 1;
            } else if referenced.contains(&b.hash) {
                r.kept_referenced += 1;
            } else if now_ms - b.mtime_ms < max_age_ms {
                r.kept_young += 1;
            } else {
                r.removed += 1;
                r.bytes += b.size;
                r.hashes.push(b.hash.clone());
                if !dry_run {
                    self.remove(&b.hash);
                }
            }
        }
        r
    }
}

/// blake3 of a file, streamed.
pub fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(path)?;
    let mut h = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 << 10];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().to_hex().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn store() -> (tempfile::TempDir, BlobStore) {
        let d = tempfile::tempdir().unwrap();
        let s = BlobStore::new(d.path().join("blobs"));
        (d, s)
    }

    #[test]
    fn put_is_content_addressed_and_idempotent() {
        let (_d, s) = store();
        let m = json!({"source": "payload"});
        let (h, p) = s.put(b"hello", "txt", &m, MetaMode::Replace).unwrap();
        assert_eq!(h, blake3::hash(b"hello").to_hex().to_string());
        assert_eq!(std::fs::read(&p).unwrap(), b"hello");
        let (h2, p2) = s.put(b"hello", "txt", &m, MetaMode::Replace).unwrap();
        assert_eq!((h, p.clone()), (h2, p2));
        assert_eq!(s.list().len(), 1);
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn if_absent_keeps_the_owner_sidecar() {
        let (_d, s) = store();
        let (h, _) = s
            .put(
                b"png",
                "png",
                &json!({"source": "screenshot", "pane": "p1"}),
                MetaMode::Replace,
            )
            .unwrap();
        s.put(
            b"png",
            "png",
            &json!({"source": "inbox"}),
            MetaMode::IfAbsent,
        )
        .unwrap();
        assert_eq!(s.find(&h).unwrap().source(), "screenshot");
        s.put(
            b"png",
            "png",
            &json!({"source": "inbox"}),
            MetaMode::Replace,
        )
        .unwrap();
        assert_eq!(s.find(&h).unwrap().source(), "inbox");
    }

    #[test]
    fn put_file_hashes_streaming_and_leaves_the_source() {
        let (d, s) = store();
        let src = d.path().join("up.bin");
        std::fs::write(&src, vec![7u8; 300_000]).unwrap();
        let (h, p) = s
            .put_file(&src, "bin", &json!({"source": "inbox"}), MetaMode::IfAbsent)
            .unwrap();
        assert_eq!(h, blake3::hash(&vec![7u8; 300_000]).to_hex().to_string());
        assert!(src.exists());
        assert_eq!(std::fs::metadata(p).unwrap().len(), 300_000);
    }

    #[test]
    fn bad_extension_falls_back_to_bin() {
        let (_d, s) = store();
        let (_, p) = s
            .put(b"x", "../etc", &Value::Null, MetaMode::Replace)
            .unwrap();
        assert!(p.to_string_lossy().ends_with(".bin"));
    }

    #[test]
    fn stats_group_by_source() {
        let (_d, s) = store();
        s.put(b"a", "bin", &json!({"source": "inbox"}), MetaMode::Replace)
            .unwrap();
        s.put(
            b"bb",
            "bin",
            &json!({"source": "payload"}),
            MetaMode::Replace,
        )
        .unwrap();
        s.put(
            b"ccc",
            "png",
            &json!({"source": "screenshot"}),
            MetaMode::Replace,
        )
        .unwrap();
        s.put(b"dddd", "bin", &Value::Null, MetaMode::Replace)
            .unwrap();
        let st = s.stats();
        assert_eq!(st.count, 4);
        assert_eq!(st.bytes, 10);
        assert_eq!(st.by_source["inbox"], (1, 1));
        assert_eq!(st.by_source["screenshot"], (1, 3));
        assert_eq!(st.by_source[""], (1, 4));
    }

    #[test]
    fn gc_only_takes_unreferenced_old_collectable_blobs() {
        let (_d, s) = store();
        let pay = json!({"source": "payload"});
        let (ha, _) = s.put(b"a", "bin", &pay, MetaMode::Replace).unwrap();
        let (hb, _) = s.put(b"b", "bin", &pay, MetaMode::Replace).unwrap();
        let (hc, _) = s
            .put(
                b"c",
                "png",
                &json!({"source": "screenshot"}),
                MetaMode::Replace,
            )
            .unwrap();
        let (hd, _) = s
            .put(b"d", "bin", &json!({"source": "inbox"}), MetaMode::Replace)
            .unwrap();
        let refs: HashSet<String> = [ha.clone()].into_iter().collect();
        let far = crate::now_ms() + 10 * 86_400_000;
        let dry = s.gc(&refs, far, 86_400_000, true);
        assert_eq!(
            (dry.removed, dry.kept_referenced, dry.kept_uncollectable),
            (2, 1, 1)
        );
        assert!(s.find(&hb).is_some(), "dry run removes nothing");
        // Too young: nothing goes.
        let young = s.gc(&refs, crate::now_ms(), 86_400_000, false);
        assert_eq!((young.removed, young.kept_young), (0, 3));
        let r = s.gc(&refs, far, 86_400_000, false);
        assert_eq!(r.removed, 2);
        assert!(s.find(&ha).is_some() && s.find(&hc).is_some());
        assert!(s.find(&hb).is_none() && s.find(&hd).is_none());
    }

    #[test]
    fn find_rejects_bad_hashes() {
        let (_d, s) = store();
        assert!(s.find("../../etc/passwd").is_none());
        assert!(s.find(&"A".repeat(64)).is_none());
        assert_eq!(s.remove("nope"), 0);
    }
}
