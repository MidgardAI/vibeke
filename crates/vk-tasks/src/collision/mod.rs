//! The shared-cwd collision tracker's pure part (05 §10): rules, claims, attribution, signal
//! extraction and the `git status` poll. The server owns watching, state and the API
//! (`vk-server::collision`).
//!
//! Collision detection is advisory. A touch is evidence that a run attempted or reported an edit
//! (or that a path changed while it worked), not proof of who owns a file's content.

mod engine;
pub mod glob;
pub mod signals;

pub use engine::{
    Attribution, Claim, CollisionRec, Finding, Kind, Merge, PathHit, Reason, Rules, RunView,
    Severity, Source, Status, TIMELINE_MAX, TimelineEntry, Touch, Tracker, attribute, dir_key,
    path_set_key,
};
pub use glob::{glob_match, normalize, valid_pattern};
pub use signals::{
    Op, StatusEntry, edit_paths, fingerprint, is_edit_tool, is_ignored_path, parse_porcelain_z,
    patch_paths, read_paths, relativize, repo_root_of, status_changes, status_op,
};
