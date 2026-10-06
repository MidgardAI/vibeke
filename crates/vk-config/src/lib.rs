//! Vibeke configuration: typed schema (08 §11), loading with located errors, key-conflict
//! checks, hot-reload diffing and a debounced file watcher.

mod binding;
mod keys;
mod load;
#[macro_use]
mod types;
mod preview;
mod units;
mod watch;

pub use binding::{Binding, parse_binding, parse_prefix_key};
pub use keys::{
    ACTION_ALIASES, COPY_MODE_ACTIONS, Conflict, ConflictReason, DEFAULT_KEYMAP, binding_clash,
    canonical_action, check_keys, default_bindings, is_copy_mode_action, is_known_action,
};
pub use load::{
    ConfigError, Diagnostic, EXTERNAL_SECTIONS, Pos, Warning, config_path, config_path_with,
    default_config_toml, default_config_toml_uncommented, requires_new_panes,
};
pub use preview::{
    AutoDiscover, BrowserExternal, LocalBrowser, PREVIEW_KEYS, PROFILE_BROWSERS, PaneLocation,
    PaneSplit, Preview, PreviewMode, ProfileRoute, ProfileScope, ScreenshotFormat, parse_viewport,
};
pub use types::*;
pub use units::{ByteSize, Dur, PortRange};
pub use watch::{ConfigWatcher, ReloadEvent, watch};
