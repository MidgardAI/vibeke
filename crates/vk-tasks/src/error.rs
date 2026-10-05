use std::path::PathBuf;

/// Crate error type.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("git {args} failed (exit {code:?}): {stderr}")]
    Git {
        args: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("git {args} timed out after {secs:.1}s")]
    Timeout { args: String, secs: f32 },
    #[error("not a git repository: {0}")]
    NotARepo(PathBuf),
    #[error("not a linked worktree: {0}")]
    NotLinkedWorktree(PathBuf),
    #[error("branch {branch} is already checked out at {path}")]
    BranchInUse { branch: String, path: PathBuf },
    #[error("path already exists: {0}")]
    PathExists(PathBuf),
    #[error("no worktree found for {0}")]
    WorktreeNotFound(String),
    #[error("refusing: {0}")]
    Refused(String),
    #[error("invalid configuration: {0}")]
    Config(String),
    #[error("no free port block available in the pool")]
    PortsExhausted,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;
