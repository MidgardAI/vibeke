//! Test-only support for the real Herdr plugin conformance harness (spec 18).
//!
//! The harness runs popular Herdr plugins, unchanged, against Vibeke's compatibility layer and
//! against the pinned Herdr baseline. This crate holds the parts that do not need a running
//! server, so they can be unit-tested on their own:
//!
//! * [`lock`]: the locked plugin corpus (`tests/compat/herdr/0.9.3/plugins/top20.lock.toml`).
//! * [`scenario`], [`coverage`]: hand-written scenarios and the check that they cover every
//!   surface a manifest declares (18 §8, §8.1).
//! * [`outcome`], [`result`]: step outcomes, the attribution rule and the per-plugin result file
//!   (18 §8.4, §12).
//! * [`trace`], [`normalize`]: `VIBEKE_HERDR_TRACE` lines, expected calls and the per-scenario
//!   normalizer (18 §9, §11).
//! * [`grid`], [`mask`], [`diff`], [`render`]: the canonical cell grid, masks and stability, grid
//!   diffs and the side-by-side report (18 §10).
//! * [`pty`], [`herdr`]: the PTY driver and the Herdr side of a run (18 §10.1, §11).
//!
//! Nothing here runs plugin code. The integration test
//! `crates/vibeke/tests/it/compat_herdr_real_plugins.rs` does that, only inside a disposable
//! execution boundary (18 §5).

pub mod coverage;
pub mod diff;
pub mod grid;
pub mod herdr;
pub mod lock;
pub mod mask;
pub mod normalize;
pub mod outcome;
pub mod pty;
pub mod render;
pub mod result;
pub mod scenario;
pub mod trace;
