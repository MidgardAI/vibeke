//! Placeholder: implemented in the provider lane (spec 17 §4.3).

use std::path::PathBuf;

use crate::unimplemented_provider;

/// Set to a directory to enable the `fake` provider (tests).
pub const DIR_ENV: &str = "VIBEKE_CLOUD_FAKE_DIR";

pub struct Fake {
    pub dir: PathBuf,
}

impl Fake {
    pub fn new(dir: PathBuf) -> Self {
        Fake { dir }
    }
}

unimplemented_provider!(Fake, "fake", "Fake (tests)");
