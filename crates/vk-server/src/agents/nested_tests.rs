use super::nested::{Proc, ancestors, harness_of, under};
use std::collections::HashMap;
use vk_hold::procinfo::ProcInfo;

fn p(pid: u32, ppid: u32, exe: &str) -> ProcInfo {
    ProcInfo {
        pid,
        ppid,
        exe: Some(exe.to_string()),
        start: u64::from(pid) * 10,
        ..Default::default()
    }
}

/// vk-hold (100) → pane zsh (200) → claude (300) → bash tool zsh (400) → codex (500) → hook sh (600).
fn table() -> HashMap<u32, ProcInfo> {
    [
        p(100, 1, "/usr/local/bin/vk-hold"),
        p(200, 100, "/bin/zsh"),
        p(300, 200, "/Users/u/.local/share/claude/versions/2.1.296"),
        p(310, 300, "/bin/sh"),
        p(400, 300, "/bin/zsh"),
        p(500, 400, "/opt/homebrew/bin/codex"),
        p(600, 500, "/bin/sh"),
        p(700, 200, "/opt/homebrew/bin/codex"),
    ]
    .into_iter()
    .map(|x| (x.pid, x))
    .collect()
}

fn chain(pid: u32) -> Vec<ProcInfo> {
    let t = table();
    ancestors(pid, |x| t.get(&x).cloned())
}

#[test]
fn harness_behind_a_hook_skips_shells() {
    // Claude's hook runs through `sh -c`: the harness is claude, below the pane shell.
    assert_eq!(
        harness_of(&chain(310), 200),
        Some(Proc {
            pid: 300,
            start: 3000
        })
    );
}

#[test]
fn harness_outside_the_pane_is_not_recorded() {
    assert_eq!(harness_of(&chain(310), 999), None);
}

#[test]
fn agent_started_by_the_pane_agent_is_nested() {
    let claude = harness_of(&chain(310), 200).unwrap();
    assert!(under(&chain(600), claude));
}

#[test]
fn agent_started_from_the_pane_shell_is_not_nested() {
    let claude = harness_of(&chain(310), 200).unwrap();
    assert!(!under(&chain(700), claude));
}

#[test]
fn reused_pid_is_not_the_recorded_process() {
    let stale = Proc { pid: 300, start: 1 };
    assert!(!under(&chain(600), stale));
}

#[test]
fn pane_command_itself_can_be_the_harness() {
    // `agent.start`: claude is the pane's child process, no shell in between.
    let t: HashMap<u32, ProcInfo> = [
        p(100, 1, "vk-hold"),
        p(300, 100, "claude"),
        p(310, 300, "/bin/sh"),
    ]
    .into_iter()
    .map(|x| (x.pid, x))
    .collect();
    let c = ancestors(310, |x| t.get(&x).cloned());
    assert_eq!(harness_of(&c, 300).map(|x| x.pid), Some(300));
}
