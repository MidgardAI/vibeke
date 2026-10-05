//! Materializing untracked files into a new worktree (05 §5, `copy` only).

use crate::Result;
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
}

/// Copy each relative path in `files` from `src_root` to `dst_root` if it
/// exists, preserving permissions and never overwriting.
pub fn copy_files(src_root: &Path, dst_root: &Path, files: &[String]) -> Result<Vec<CopyResult>> {
    let mut results = Vec::new();
    for rel in files {
        let outcome = copy_one(src_root, dst_root, rel);
        results.push(CopyResult {
            rel: rel.clone(),
            outcome,
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
