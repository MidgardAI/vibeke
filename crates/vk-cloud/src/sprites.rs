//! Placeholder: implemented in the provider lane (spec 17 §4).

use crate::{ProviderConfig, unimplemented_provider};

pub struct Sprites {
    pub cfg: ProviderConfig,
}

impl Sprites {
    pub fn new(cfg: ProviderConfig) -> Self {
        Sprites { cfg }
    }
}

unimplemented_provider!(Sprites, "sprites", "Sprites");
