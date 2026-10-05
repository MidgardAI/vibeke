//! `vibeke doctor` and `vibeke update`. Filled in by the switch-over stage.

use vk_cli::{EXIT_USAGE, Global};

pub async fn run(_g: &Global, _args: &[String]) -> i32 {
    eprintln!("vibeke doctor: not available yet");
    EXIT_USAGE
}

pub async fn update(_g: &Global, _args: &[String]) -> i32 {
    eprintln!("vibeke update: not available yet");
    EXIT_USAGE
}
