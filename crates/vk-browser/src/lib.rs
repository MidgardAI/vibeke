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
//!
//! The agents' headless browser (spec 06 B5, Goal 03 Stage 3):
//!
//! - [`policy`]: destination classes and decisions (resolved-IP checks).
//! - [`proxy`]: the per-session filtering HTTP/CONNECT proxy (resolve once, pin).
//! - [`headless`]: headless binary discovery and the network-isolation launch flags.
//! - [`install`]: `vibeke browser install` (pinned Chrome for Testing, SHA-256 verified).
//! - [`snapshot`]: accessibility-tree text snapshots.
//! - [`fake`]: a fake CDP browser for tests.

pub mod cdp;
pub mod fake;
/// Fake Chromium speaking CDP over the pipe (browser-pane tests, `vibeke debug fake-chromium`).
pub mod fake_chromium;
pub mod frame;
pub mod headless;
pub mod input;
pub mod install;
pub mod kitty;
pub mod local_http;
pub mod policy;
pub mod probe;
pub mod proxy;
pub mod snapshot;
