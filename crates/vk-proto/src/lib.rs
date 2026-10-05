//! Wire types shared by every Vibeke process.
//!
//! - [`holder`]: server ⇄ `vibeke hold` protocol (`holder/1`, 07 §4).
//! - [`render`]: render stream frames (07 §3).
//! - [`rpc`]: JSON-RPC 2.0 envelopes and error kinds (07 §1).
//! - [`input`]: logical input events (03 §7).
//! - [`frame`]: `u32 LE length | postcard payload` framing used by holder, render and bridge links.

pub mod frame;
pub mod holder;
pub mod input;
pub mod layout;
pub mod model;
pub mod render;
pub mod rpc;

/// Control API version string (01 §7.3).
pub const API_VERSION: &str = "vibeke/1";

/// Crate version of the running binary.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Replies to terminal identification queries. Shared by the holder (answers while no server
/// is attached, 01 §1.2) and the server's VT engine so both identify identically.
pub mod ident {
    /// DA1: VT220-class with ANSI colour (22) and OSC 52 clipboard (52).
    pub const DA1: &str = "\x1b[?62;22;52c";
    /// DA2: VT220 type, firmware version 100.
    pub const DA2: &str = "\x1b[>1;100;0c";
    /// DA3: unit id.
    pub const DA3: &str = "\x1bP!|56494245\x1b\\";
    pub fn xtversion() -> String {
        format!("\x1bP>|vibeke {}\x1b\\", crate::VERSION)
    }
}
