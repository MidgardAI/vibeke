//! The PTY driver (18 §10.1): runs a program in a pseudo-terminal, decodes its output with
//! `vk-term`, writes `Effect::Reply` bytes (terminal query answers) back to the PTY, sends
//! input, resizes, waits for readiness and shuts down cleanly.

use std::path::Path;
use std::time::Duration;

pub struct PtyDriver {
    // lane GRID: master fd, child pid, reader thread, Arc<Mutex<vk_term::Engine>>, …
}

pub struct Spawn<'a> {
    pub program: &'a Path,
    pub args: &'a [String],
    pub cwd: &'a Path,
    /// The complete environment (an allowlist; nothing is inherited).
    pub env: &'a [(String, String)],
    pub cols: u16,
    pub rows: u16,
}

impl PtyDriver {
    pub fn spawn(spec: &Spawn<'_>) -> anyhow::Result<PtyDriver> {
        todo!("lane GRID")
    }

    pub fn write(&self, bytes: &[u8]) -> anyhow::Result<()> {
        todo!("lane GRID")
    }

    pub fn resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        todo!("lane GRID")
    }

    /// The current screen as `vk-term` rows, plus the palette changes the program made.
    pub fn snapshot(
        &self,
    ) -> (
        Vec<vk_proto::render::Row>,
        u16,
        u16,
        crate::grid::PaletteChanges,
    ) {
        todo!("lane GRID")
    }

    /// Wait until the screen text contains `needle`.
    pub fn wait_for_text(&self, needle: &str, timeout: Duration) -> anyhow::Result<()> {
        todo!("lane GRID")
    }

    /// SIGTERM the process group, wait up to `grace`, then SIGKILL. Best effort (18 §5.4).
    pub fn shutdown(self, grace: Duration) -> anyhow::Result<Option<i32>> {
        todo!("lane GRID")
    }
}
