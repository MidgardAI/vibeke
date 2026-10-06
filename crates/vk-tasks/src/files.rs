//! Materializing untracked files into a new worktree (05 §5): `copy`,
//! `link` (symlink to the source checkout) and `clone` (copy-on-write).

use crate::Result;
use crate::clone::{CloneMethod, clone_path};
use crate::taskfile::FilesSpec;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Component, Path};

/// Default `tasks.copy_files`.
pub fn default_copy_files() -> Vec<String> {
    vec![".env".into(), ".env.local".into()]
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CopyOutcome {
    Copied,
    /// Symlinked to the source checkout.
    Linked,
    /// Cloned (copy-on-write where the filesystem allows, else copied).
    Cloned(CloneMethod),
    /// Source does not exist (not an error).
    MissingSource,
    /// Destination already exists; never overwritten.
    DestExists,
    /// Source is not a regular file, or the path is absolute / escapes the root.
    Rejected(String),
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyResult {
    /// Path relative to both roots. Only names are reported, never contents.
    pub rel: String,
    pub outcome: CopyOutcome,
    /// blake3 of a copied file's contents (names and hashes only are ever
    /// reported, never contents).
    pub hash: Option<String>,
}

/// Copy each relative path in `files` from `src_root` to `dst_root` if it
/// exists, preserving permissions and never overwriting.
pub fn copy_files(src_root: &Path, dst_root: &Path, files: &[String]) -> Result<Vec<CopyResult>> {
    let mut results = Vec::new();
    for rel in files {
        let outcome = copy_one(src_root, dst_root, rel);
        let hash = matches!(outcome, CopyOutcome::Copied)
            .then(|| fs::read(dst_root.join(rel)).ok())
            .flatten()
            .map(|b| blake3::hash(&b).to_hex().to_string());
        results.push(CopyResult {
            rel: rel.clone(),
            outcome,
            hash,
        });
    }
    Ok(results)
}

fn copy_one(src_root: &Path, dst_root: &Path, rel: &str) -> CopyOutcome {
    let rel_path = Path::new(rel);
    if rel.is_empty()
        || rel_path.is_absolute()
        || rel_path
            .components()
            .any(|c| !matches!(c, Component::Normal(_)))
    {
        return CopyOutcome::Rejected("path must be relative and stay inside the repo".into());
    }
    let src = src_root.join(rel_path);
    let dst = dst_root.join(rel_path);
    let meta = match fs::metadata(&src) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return CopyOutcome::MissingSource,
        Err(e) => return CopyOutcome::Failed(e.to_string()),
    };
    if !meta.is_file() {
        return CopyOutcome::Rejected("not a regular file".into());
    }
    if fs::symlink_metadata(&dst).is_ok() {
        return CopyOutcome::DestExists;
    }
    let run = || -> io::Result<CopyOutcome> {
        if let Some(p) = dst.parent() {
            fs::create_dir_all(p)?;
        }
        let mut out = match OpenOptions::new().write(true).create_new(true).open(&dst) {
            Ok(f) => f,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Ok(CopyOutcome::DestExists);
            }
            Err(e) => return Err(e),
        };
        io::copy(&mut fs::File::open(&src)?, &mut out)?;
        fs::set_permissions(&dst, meta.permissions())?;
        Ok(CopyOutcome::Copied)
    };
    run().unwrap_or_else(|e| CopyOutcome::Failed(e.to_string()))
}

/// Expand `*` / `?` segments of a repo-relative pattern against `root`
/// (sorted, never descending into `.git`, hidden entries only when the
/// pattern segment itself starts with `.`). A pattern without wildcards is
/// returned as is, so a missing literal path can be reported.
pub fn expand_glob(root: &Path, pattern: &str) -> Vec<String> {
    if !pattern.contains(['*', '?']) {
        return vec![pattern.to_string()];
    }
    let mut cur: Vec<String> = vec![String::new()];
    for seg in pattern.split('/').filter(|s| !s.is_empty()) {
        let mut next = Vec::new();
        for base in &cur {
            let join = |n: &str| {
                if base.is_empty() {
                    n.to_string()
                } else {
                    format!("{base}/{n}")
                }
            };
            if !seg.contains(['*', '?']) {
                next.push(join(seg));
                continue;
            }
            let Ok(rd) = fs::read_dir(root.join(base)) else {
                continue;
            };
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name == ".git" || (name.starts_with('.') && !seg.starts_with('.')) {
                    continue;
                }
                if glob_seg(seg.as_bytes(), name.as_bytes()) {
                    next.push(join(&name));
                }
            }
        }
        cur = next;
    }
    cur.sort();
    // Keep only what exists; wildcards that match nothing yield nothing.
    cur.retain(|r| fs::symlink_metadata(root.join(r)).is_ok());
    cur
}

fn glob_seg(p: &[u8], n: &[u8]) -> bool {
    match (p.first(), n.first()) {
        (None, None) => true,
        (Some(b'*'), _) => glob_seg(&p[1..], n) || (!n.is_empty() && glob_seg(p, &n[1..])),
        (Some(b'?'), Some(_)) => glob_seg(&p[1..], &n[1..]),
        (Some(a), Some(b)) if a == b => glob_seg(&p[1..], &n[1..]),
        _ => false,
    }
}

fn safe_rel(rel: &str) -> std::result::Result<&Path, String> {
    let p = Path::new(rel);
    if rel.is_empty()
        || p.is_absolute()
        || p.components().any(|c| !matches!(c, Component::Normal(_)))
    {
        return Err("path must be relative and stay inside the repo".into());
    }
    Ok(p)
}

fn link_one(src_root: &Path, dst_root: &Path, rel: &str) -> CopyOutcome {
    let p = match safe_rel(rel) {
        Ok(p) => p,
        Err(e) => return CopyOutcome::Rejected(e),
    };
    let (src, dst) = (src_root.join(p), dst_root.join(p));
    if fs::symlink_metadata(&src).is_err() {
        return CopyOutcome::MissingSource;
    }
    if fs::symlink_metadata(&dst).is_ok() {
        return CopyOutcome::DestExists;
    }
    let run = || -> io::Result<()> {
        if let Some(d) = dst.parent() {
            fs::create_dir_all(d)?;
        }
        std::os::unix::fs::symlink(&src, &dst)
    };
    match run() {
        Ok(()) => CopyOutcome::Linked,
        Err(e) => CopyOutcome::Failed(e.to_string()),
    }
}

fn clone_one(src_root: &Path, dst_root: &Path, rel: &str) -> CopyOutcome {
    let p = match safe_rel(rel) {
        Ok(p) => p,
        Err(e) => return CopyOutcome::Rejected(e),
    };
    let (src, dst) = (src_root.join(p), dst_root.join(p));
    if fs::symlink_metadata(&src).is_err() {
        return CopyOutcome::MissingSource;
    }
    match clone_path(&src, &dst) {
        Ok(m) => CopyOutcome::Cloned(m),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => CopyOutcome::DestExists,
        Err(e) => CopyOutcome::Failed(e.to_string()),
    }
}

/// Materialize `spec` from `src_root` into `dst_root`: `copy`, then `link`,
/// then `clone`. Globs are expanded against the source. With
/// `ignore_missing = false` a missing source is reported as a failure.
/// Existing destinations are never overwritten, nothing is ever deleted.
pub fn materialize_files(src_root: &Path, dst_root: &Path, spec: &FilesSpec) -> Vec<CopyResult> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    type Op = fn(&Path, &Path, &str) -> CopyOutcome;
    let groups: [(&Vec<String>, Op); 3] = [
        (&spec.copy, copy_one),
        (&spec.link, link_one),
        (&spec.clone, clone_one),
    ];
    for (patterns, op) in groups {
        for pat in patterns {
            let mut rels = expand_glob(src_root, pat);
            if rels.is_empty() {
                rels.push(pat.clone());
            }
            for rel in rels {
                if !seen.insert(rel.clone()) {
                    continue;
                }
                let mut outcome = op(src_root, dst_root, &rel);
                if matches!(outcome, CopyOutcome::MissingSource) && !spec.ignore_missing() {
                    outcome = CopyOutcome::Failed("source does not exist".into());
                }
                let hash = matches!(outcome, CopyOutcome::Copied)
                    .then(|| fs::read(dst_root.join(&rel)).ok())
                    .flatten()
                    .map(|b| blake3::hash(&b).to_hex().to_string());
                out.push(CopyResult { rel, outcome, hash });
            }
        }
    }
    out
}
