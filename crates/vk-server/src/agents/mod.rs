//! Harness adapters (04): process detection, hook transport, state arbitration, interactions
//! with the delivery state machine. (Filled in by the agents stage.)

use crate::Server;
use crate::api::{Ctx, R};
use crate::core::{Core, Tx};
use serde_json::Value;
use std::sync::Arc;
use vk_proto::holder::ProcStatus;
use vk_proto::model::AgentRun;

pub const METHODS: &[(&str, bool)] = &[];

#[derive(Default)]
pub struct Agents {}

pub fn start(_server: &Arc<Server>) {}

pub async fn api(_server: &Arc<Server>, _ctx: &Ctx, _method: &str, _p: &Value) -> Option<R> {
    None
}

pub async fn start_in_pane(
    _server: &Arc<Server>,
    _pane: &str,
    _harness: &str,
    _name: Option<&str>,
    _prompt: Option<&str>,
    _args: &[String],
    _task: Option<&str>,
) -> Result<Value, vk_proto::rpc::RpcError> {
    Err(crate::api::err(
        vk_proto::rpc::ErrorKind::Unsupported,
        "agents not available yet",
    ))
}

impl Agents {
    pub fn input_blocked(&self, _pane: &str) -> Option<&'static str> {
        None
    }
    pub fn on_focus(&self, _server: &Server, _pane: &str) {}
    pub fn on_process(&self, _server: &Arc<Server>, _pane: &str, _st: &ProcStatus) {}
    pub fn end_run(&self, _server: &Server, _run: &str, _reason: &str) {}
    pub fn end_run_tx(&self, _core: &mut Core, _tx: &mut Tx, _run: &AgentRun, _reason: &str) {}
}

pub mod hook {
    /// `vibeke hook <harness> <event>` — filled in by the agents stage. Observation fails open.
    pub fn main(_args: &[String]) -> i32 {
        0
    }
}

/// Write PATH shims (codex → per-pane embedded app-server, 04 §6.2).
pub fn install_shims(_bin: &std::path::Path) -> std::io::Result<()> {
    Ok(())
}
