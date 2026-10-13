//! Harness integration: hook installers (Claude Code, Codex), the Codex PATH
//! shim, the Phase 1 risk heuristic and approval fingerprints.
//! See `spec/04-harness-adapters.md` (§6, §7.6, §7.7, §11).

pub mod fingerprint;
mod hermes_config;
pub mod install;
pub mod manifest;
pub mod mcp;
pub mod pricing;
pub mod risk;
pub(crate) mod shell;
pub mod skill;
pub mod transcript;

pub use fingerprint::{Subject, fingerprint, fingerprint_subject};
pub use install::{
    Dirs, FileChange, Harness, HookStatus, InstallState, Plan, PlanKind, Status, Trust, apply,
    hook_bin, hook_status, hooks_present, is_build_output, plan_install, plan_uninstall,
    refresh_managed, stale_command, stale_hooks, status, write_codex_shim,
};
pub use mcp::{McpStatus, mcp_config_file, mcp_status, plan_mcp_install, plan_mcp_uninstall};
pub use risk::{Risk, assess};
pub use skill::{
    REMOTE_SKILL, SKILL, SkillState, remote_skill_file, remote_skill_state, skill_file, skill_state,
};
