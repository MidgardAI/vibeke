//! Placeholder: implemented in the provider lane (spec 17 §4).

use crate::{ProviderConfig, unimplemented_provider};

pub struct E2b {
    pub cfg: ProviderConfig,
}

impl E2b {
    pub fn new(cfg: ProviderConfig) -> Self {
        E2b { cfg }
    }
}

unimplemented_provider!(E2b, "e2b", "E2b");
