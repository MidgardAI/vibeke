//! Terminal state for Vibeke panes (03): the VT engine binding, render rows, snapshots and the
//! canonical input encoder.

pub mod encode;
#[cfg(not(target_arch = "wasm32"))]
pub mod engine;
#[cfg(not(target_arch = "wasm32"))]
mod ghostty_sys;
pub mod keygrammar;
pub mod passthrough;
#[cfg(not(target_arch = "wasm32"))]
pub mod tracker;
#[cfg(not(target_arch = "wasm32"))]
pub mod vt;

#[cfg(not(target_arch = "wasm32"))]
pub use engine::{Effect, Engine, LastCommand, NotifyKind};
#[cfg(not(target_arch = "wasm32"))]
pub use vt::{EngineEffect, VtEngine};

pub mod limits;
