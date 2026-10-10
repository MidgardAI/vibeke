//! Shared terminal limits used by the server and its clients.
/// Longest OSC 8 URI kept on a row; longer links render as plain text.
pub const LINK_URI_MAX: usize = 2048;
/// Default largest decoded image a pane may store.
pub const MAX_IMAGE_BYTES: usize = 32 << 20;
/// Default kitty image storage per pane screen; older images are evicted beyond it.
pub const MAX_IMAGES_PER_PANE: u64 = 256 << 20;
/// An image's base64 APC must fit the engine snapshot continuation limit.
pub const MAX_IMAGE_BYTES_CAP: usize = 48 << 20;
