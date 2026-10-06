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
pub mod migrate;
pub mod registry;
pub mod status;
pub mod wire;

/// The Herdr version whose public contract the compat layer emulates (07 §8.0).
pub const BASELINE_VERSION: &str = "0.9.3";
/// The baseline tag's commit, resolved on 2026-10-06.
pub const BASELINE_COMMIT: &str = "7b116c05bfda646af39d2524c54e70c751f57ee8";

/// Herdr's plugin manifest file name.
pub const MANIFEST_FILE: &str = "herdr-plugin.toml";

/// The Herdr session that maps to Vibeke's default session.
pub const DEFAULT_SESSION: &str = "default";

/// Herdr's socket layout under a compat root (07 §8.3): the default session listens on
/// `<root>/herdr.sock`, a named session on `<root>/sessions/<name>/herdr.sock`. Vibeke session
/// names map one to one; `default` is Herdr's default session.
pub fn session_socket(root: &std::path::Path, session: &str) -> std::path::PathBuf {
    if session == DEFAULT_SESSION {
        root.join("herdr.sock")
    } else {
        root.join("sessions").join(session).join("herdr.sock")
    }
}

/// A session name usable as one path component (no separators, no `.`/`..`, not empty).
pub fn valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

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

    #[test]
    fn session_layout() {
        let root = std::path::Path::new("/r/herdr-compat");
        assert_eq!(
            session_socket(root, "default"),
            root.join("herdr.sock"),
            "default session at the root, like Herdr"
        );
        assert_eq!(
            session_socket(root, "work"),
            root.join("sessions/work/herdr.sock")
        );
        assert!(valid_session_name("work-2"));
        for bad in ["", "..", "a/b", "x y", "."] {
            assert!(!valid_session_name(bad), "{bad:?}");
        }
    }
}
