//! The Herdr compatibility layer (07 §7.7, §8) — first slice.
//!
//! The pinned baseline is **Herdr v0.9.3** at commit `7b116c05…` (07 §8.0). Nothing in this
//! module was checked against a running Herdr binary: shapes come from the spec's mapping tables
//! and from the 99 real plugin manifests in `tests/compat/herdr/0.9.3/manifests/`. Each
//! surface's status is recorded in [`inventory`], and `docs/herdr-compat-inventory.md` is
//! generated from it. Support is **partial** until the differential suite (07 §8.4) passes.

pub mod cli;
pub mod events;
pub mod inventory;
pub mod launch;
pub mod manifest;
pub mod registry;
pub mod status;
pub mod wire;

/// The Herdr version whose public contract the compat layer emulates (07 §8.0).
pub const BASELINE_VERSION: &str = "0.9.3";
/// The baseline tag's commit, resolved on 2026-10-06.
pub const BASELINE_COMMIT: &str = "7b116c05bfda646af39d2524c54e70c751f57ee8";

/// Herdr's plugin manifest file name.
pub const MANIFEST_FILE: &str = "herdr-plugin.toml";

/// `(major, minor, patch)` of a `x.y.z` version string; missing parts are 0, a pre-release or
/// build suffix is ignored.
pub fn parse_version(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut it = core.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    let patch = it.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    if it.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// The current platform in Herdr's manifest vocabulary (`linux`, `macos`, `windows`).
pub fn current_platform() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(target_os = "windows") {
        "windows"
    } else {
        "linux"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions() {
        assert_eq!(parse_version("0.9.3"), Some((0, 9, 3)));
        assert_eq!(parse_version("v0.7"), Some((0, 7, 0)));
        assert_eq!(parse_version("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3.4"), None);
        assert_eq!(parse_version("x"), None);
        assert!(parse_version("0.9.1") < parse_version(BASELINE_VERSION));
    }
}
