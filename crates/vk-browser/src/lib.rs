//! Browser pane groundwork (spec 06 B3.2, Goal 03 Stage 0).
//!
//! - [`cdp`]: Chrome DevTools Protocol over `--remote-debugging-pipe` (fds 3/4, NUL-delimited
//!   JSON), with a small launcher for a Chromium binary found on disk.
//! - [`frame`]: frame decoding and cell-aligned tile diffing.
//! - [`kitty`]: kitty graphics protocol output (direct/chunked, shared memory, temp file) and
//!   unicode-placeholder placement.
//! - [`probe`]: host capability queries and reply parsers (kitty graphics, cell/window pixel
//!   size, DECRQM, SGR-pixels mouse).
//! - [`local_http`]: loopback-only HTTP server for tests and the bench.
//! - [`input`]: logical key events (`vk_proto::input::KeyEvent`) → CDP `Input.*` commands.

pub mod cdp;
pub mod frame;
pub mod input;
pub mod kitty;
pub mod local_http;
pub mod probe;
