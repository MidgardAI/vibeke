//! Fuzz targets for every parser that sees untrusted bytes (spec 10 §6).
//!
//! Each target is a plain `fn(&[u8])` that must never panic, hang or allocate without bound
//! whatever it is given. Two front ends share them:
//!
//! * `tests/random.rs` runs a bounded number of random and mutated-seed cases on stable
//!   (part of `mise run ci`; `VK_FUZZ_CASES` raises the count).
//! * `fuzz/` (cargo-fuzz, nightly + libFuzzer, outside the workspace) calls the same functions.

pub mod rng;
pub mod seeds;
pub mod targets;
mod targets_extra;

pub use targets::{TARGETS, Target};
