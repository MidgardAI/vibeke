//! A `cd` in a shell that sends no OSC 7 changes no process, yet the pane's cwd and its
//! workspace's automatic name follow it without another command being run.

use crate::support::Session;
use std::time::{Duration, Instant};

#[test]
fn cd_without_osc7_moves_the_pane_cwd_and_workspace_name() {
    let s = Session::new();
    let dir = tempfile::Builder::new()
        .prefix("vkcd")
        .tempdir_in("/tmp")
        .unwrap();
    let base = dir
        .path()
        .file_name()
        .unwrap()
        .to_string_lossy()
        .to_string();
    let pane = s.workspace("/bin/sh");
    s.json(&["pane", "send-text", &pane, "echo ready-$((1+1))\n"]);
    s.wait_output(&pane, "ready-2", 5_000);
    // A builtin only: no new foreground process, so nothing but the shell's own cwd changes.
    s.json(&[
        "pane",
        "send-text",
        &pane,
        &format!("cd {}\n", dir.path().display()),
    ]);
    let ws_id = s.pane(&pane)["workspace"].as_str().unwrap().to_string();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let cwd = s.pane(&pane)["cwd"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let name = s.json(&["workspace", "list"])["workspaces"]
            .as_array()
            .and_then(|ws| ws.iter().find(|w| w["id"] == ws_id.as_str()).cloned())
            .and_then(|w| w["name"].as_str().map(str::to_string))
            .unwrap_or_default();
        if cwd.ends_with(&base) && name.ends_with(&base) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cwd {cwd:?} / workspace name {name:?} never followed the cd to {base}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}
