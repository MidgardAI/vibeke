//! `vibeke integration install|status|uninstall|doctor` (04 §11). Filled in by the agents stage.

use vk_cli::{EXIT_USAGE, Global};

pub async fn run(_g: &Global, _args: &[String]) -> i32 {
    eprintln!("vibeke integration: not available yet");
    EXIT_USAGE
}
