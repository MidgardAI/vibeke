//! Agents started by the pane's agent (Claude running `codex exec` for a review) inherit the
//! pane's environment, so their hooks reach the server as signals of that pane. Without this
//! check such a signal replaces the pane's run with a run of the nested harness, and the outer
//! run comes back as a new run that never sees its SessionStart.
//!
//! The hook shim sends its parent pid (`adapter.signal` `pid`). The harness process behind a
//! hook is the nearest ancestor that is not a shell (hooks run through `sh -c`). It is recorded
//! per pane when it runs under the pane's child process. A signal of a different harness whose
//! ancestors include the recorded, still running harness process comes from a nested agent and
//! is dropped. A missing or foreign pid (another pid namespace) only ever disables the check.

use super::harness::Harness;
use crate::Server;
use vk_hold::procinfo::{self, ProcInfo};

/// Ancestor walks stop after this many steps.
const MAX_DEPTH: usize = 32;
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "fish", "ksh", "mksh", "tcsh", "csh",
];

/// A process identity that survives pid reuse.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Proc {
    pub pid: u32,
    pub start: u64,
}

impl Proc {
    fn of(p: &ProcInfo) -> Self {
        Proc {
            pid: p.pid,
            start: p.start,
        }
    }
}

/// `pid` and its ancestors, nearest first (stops before pid 1).
pub(super) fn ancestors(pid: u32, info: impl Fn(u32) -> Option<ProcInfo>) -> Vec<ProcInfo> {
    let mut out = Vec::new();
    let mut cur = pid;
    while cur > 1 && out.len() < MAX_DEPTH {
        let Some(p) = info(cur) else { break };
        cur = p.ppid;
        out.push(p);
    }
    out
}

fn is_shell(p: &ProcInfo) -> bool {
    let name = p
        .exe
        .as_deref()
        .or(p.argv.first().map(String::as_str))
        .unwrap_or("");
    let base = name.rsplit('/').next().unwrap_or(name);
    SHELLS.contains(&base.trim_start_matches('-'))
}

/// The harness process behind a hook, from the hook's ancestor chain: the nearest non-shell
/// process, provided the chain reaches the pane's child process (`pane_child`).
pub(super) fn harness_of(chain: &[ProcInfo], pane_child: u32) -> Option<Proc> {
    let within = chain.iter().position(|p| p.pid == pane_child)?;
    chain[..=within].iter().find(|p| !is_shell(p)).map(Proc::of)
}

/// Whether a chain contains the process `outer`.
pub(super) fn under(chain: &[ProcInfo], outer: Proc) -> bool {
    chain.iter().any(|p| Proc::of(p) == outer)
}

fn pane_child(server: &Server, pane: &str) -> Option<u32> {
    server
        .with_core(|c| c.pane(pane).and_then(|p| p.child_pid))
        .filter(|&p| p > 1)
}

/// True when a signal of `h` from hook parent `pid` comes from an agent nested in the pane's
/// running agent of another harness: the caller drops it.
pub(super) fn is_nested(server: &Server, pane: &str, h: Harness, pid: Option<u32>) -> bool {
    let Some(pid) = pid else { return false };
    let Some(run_harness) = server.with_core(|c| c.run_for_pane(pane).map(|r| r.harness.clone()))
    else {
        return false;
    };
    if run_harness == h.id() {
        return false;
    }
    let Some((rec_harness, outer)) = server.agents.harness_proc(pane) else {
        return false;
    };
    if rec_harness != run_harness {
        return false;
    }
    // The outer agent exited (or its pid was reused): a new agent in the pane, not a nested one.
    if procinfo::info(outer.pid).is_none_or(|p| p.start != outer.start) {
        return false;
    }
    under(&ancestors(pid, procinfo::info), outer)
}

/// Remember the harness process of `h` in `pane` after its signal was routed. Recomputed only
/// when nothing (or another harness) is recorded, or the recorded process is gone.
pub(super) fn record(server: &Server, pane: &str, h: Harness, pid: Option<u32>, event: &str) {
    let Some(pid) = pid else { return };
    let fresh = event == "SessionStart";
    if !fresh
        && let Some((rec, p)) = server.agents.harness_proc(pane)
        && rec == h.id()
        && procinfo::info(p.pid).is_some_and(|i| i.start == p.start)
    {
        return;
    }
    let Some(child) = pane_child(server, pane) else {
        return;
    };
    if let Some(p) = harness_of(&ancestors(pid, procinfo::info), child) {
        server.agents.set_harness_proc(pane, h.id().to_string(), p);
    }
}
