//! Clock shared by the native and browser clients.
#[cfg(not(target_arch = "wasm32"))]
pub use std::time::*;
#[cfg(target_arch = "wasm32")]
pub use web_time::*;
