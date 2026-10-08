//! Harness integration: hook installers (Claude Code, Codex), the Codex PATH
//! shim, the Phase 1 risk heuristic and approval fingerprints.
//! See `spec/04-harness-adapters.md` (§6, §7.6, §7.7, §11).

pub mod fingerprint;
pub mod install;
pub mod manifest;
pub mod mcp;
pub mod pricing;
pub mod risk;
pub(crate) mod shell;
pub mod transcript;

pub use fingerprint::{Subject, fingerprint, fingerprint_subject};
pub use install::{
    Dirs, FileChange, Harness, HookStatus, InstallState, Plan, PlanKind, Status, Trust, apply,
    hook_bin, is_build_output, plan_install, plan_uninstall, stale_command, status,
    write_codex_shim,
};
pub use mcp::{McpStatus, mcp_config_file, mcp_status, plan_mcp_install, plan_mcp_uninstall};
pub use risk::{Risk, assess};
