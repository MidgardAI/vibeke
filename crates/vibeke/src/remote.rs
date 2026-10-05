//! Remote machines over SSH (06 Part A). Filled in by the remote stage.

use vk_cli::{EXIT_USAGE, Global};

pub async fn bridge(_g: &Global, _args: &[String]) -> i32 {
    eprintln!("vibeke bridge: not available yet");
    EXIT_USAGE
}

pub async fn ssh(_g: &Global, _args: &[String]) -> i32 {
    eprintln!("vibeke ssh: not available yet");
    EXIT_USAGE
}

pub fn specs(_cfg: &vk_config::Config, _g: &Global) -> Vec<vk_tui::app::MachineSpec> {
    vec![]
}
