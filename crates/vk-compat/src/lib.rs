//! Herdr compatibility: config and session importers (08 §12) and the M5 compatibility layer
//! (07 §7.7, §8): plugin manifests, the per-user plugin registry with legacy trust, the Herdr
//! wire protocol, event projection, the plugin invocation environment, the `herdr` CLI shim
//! grammar and the baseline inventory. Everything here is pure (no server state); the server
//! side lives in `vk-server/src/compat.rs`.

mod config;
pub mod herdr;
pub mod native;
mod session;

pub use config::{ImportReport, import_config, write_imported};
pub use session::{
    AgentRef, Layout, Leaf, Orientation, ResumeCandidate, SessionImportError, SessionPlan,
    SessionRefKind, Tab, Workspace, import_session,
};
