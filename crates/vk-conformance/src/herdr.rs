//! The Herdr side of a run (18 §10.2, §11): the pinned reference binary in its own temp HOME,
//! XDG, TMPDIR and runtime dirs, its TUI in a [`crate::pty::PtyDriver`], its socket behind a
//! recording proxy, and the crop rules that cut a plugin pane out of Herdr's screen.

use crate::grid::{Grid, Rect};
use std::path::{Path, PathBuf};

pub const BASELINE: &str = "0.9.3";
pub const TAG_COMMIT: &str = "7b116c05bfda646af39d2524c54e70c751f57ee8";

/// Verify the binary's sha256 against `expected` BEFORE running it at all, then check that
/// `--version` reports [`BASELINE`]. Never resolves `herdr` from PATH.
pub fn verify_binary(bin: &Path, expected_sha256: &str) -> anyhow::Result<()> {
    todo!("lane GRID")
}

pub struct HerdrSide {
    // lane GRID: temp root, env, driver, proxy thread, socket paths, trace file, …
}

pub struct HerdrOptions {
    pub bin: PathBuf,
    /// Root for HOME, XDG_*, TMPDIR and the runtime dir; created fresh by the caller.
    pub root: PathBuf,
    /// The outer terminal size of Herdr's TUI.
    pub cols: u16,
    pub rows: u16,
    /// Trace file the recording proxy appends [`crate::trace::TraceLine`]s to.
    pub trace: PathBuf,
    /// Extra environment for plugin processes (stubs), already allowlisted.
    pub env: Vec<(String, String)>,
}

impl HerdrSide {
    pub fn start(opts: HerdrOptions) -> anyhow::Result<HerdrSide> {
        todo!("lane GRID")
    }

    /// Run the Herdr CLI (`herdr <args>`) against this side, returning (exit code, stdout,
    /// stderr).
    pub fn cli(&self, args: &[&str]) -> anyhow::Result<(i32, String, String)> {
        todo!("lane GRID")
    }

    /// One JSON request on Herdr's socket through the proxy.
    pub fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        todo!("lane GRID")
    }

    /// The plugin pane's rectangle from `pane.layout`, including its frame.
    pub fn pane_rect(&self, pane: &str) -> anyhow::Result<Rect> {
        todo!("lane GRID")
    }

    /// Capture Herdr's screen and crop the plugin pane's inner area: `pane.layout` rectangle
    /// minus the frame, with separate rules for zoomed panes, overlays and popups.
    pub fn capture_pane(&self, pane: &str, placement: &str) -> anyhow::Result<Grid> {
        todo!("lane GRID")
    }

    pub fn resize_pane(&self, pane: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        todo!("lane GRID")
    }

    pub fn stop(self) -> anyhow::Result<()> {
        todo!("lane GRID")
    }
}

/// The crop rule for one placement, given the `pane.layout` rectangle (frame included).
pub fn crop_rect(layout: Rect, placement: &str) -> Rect {
    todo!("lane GRID")
}
