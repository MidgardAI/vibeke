//! Native Vibeke plugins (07 §7.1–7.6, 09 §6): the `vibeke-plugin.toml` manifest, the
//! capability model with risk levels and widening detection, native registrations (stored in
//! the shared per-user `plugins.json`, next to the Herdr ones), the commit-pinned
//! `plugins.lock`, the marketplace index reader (`plugin search`) and plugin backup/export.
//!
//! Everything here is pure (no server state); the runtime (process plugins, capability-scoped
//! tokens, KV, UI contributions, dev-link hot restart) lives in `vk-server/src/plugin_native/`.
//! Herdr plugins keep their own manifest and legacy trust path ([`crate::herdr`]); a directory
//! with a `vibeke-plugin.toml` is a native plugin even when it also carries a
//! `herdr-plugin.toml`.

pub mod backup;
pub mod caps;
pub mod index;
pub mod lockfile;
pub mod manifest;
pub mod registry;

use std::path::Path;

/// The native manifest file name (07 §7.1).
pub const MANIFEST_FILE: &str = "vibeke-plugin.toml";

/// Does `dir` (or the directory of a manifest path) hold a native plugin manifest?
pub fn is_native(src: &Path) -> bool {
    if src.file_name().and_then(|n| n.to_str()) == Some(MANIFEST_FILE) {
        return src.is_file();
    }
    src.join(MANIFEST_FILE).is_file()
}
