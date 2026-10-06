//! Mouse selection end to end (03 §11.1, 08 §13 D#748): the real TUI runs in a PTY under
//! `vibeke debug ptyshot` against an isolated session; SGR mouse reports drag over shell output
//! and double-click a word, and the TUI's output stream must carry OSC 52 writes with exactly
//! that text. The platform clipboard tool is replaced by `VIBEKE_CLIPBOARD_CMD` writing to a
//! file, so the host clipboard and terminal are never touched.

use std::process::Command;

#[test]
fn drag_and_double_click_over_shell_output_write_osc52() {
    let dir = tempfile::Builder::new()
        .prefix("vksel")
        .tempdir_in("/tmp")
        .unwrap();
    let d = dir.path().canonicalize().unwrap();
    std::fs::write(
        d.join("config.toml"),
        "[terminal]\ndefault_shell = \"/bin/sh\"\n[ui.sidebar]\ncollapsed = true\n",
    )
    .unwrap();
    let clip = d.join("clip.log");
    let bin = env!("CARGO_BIN_EXE_vibeke");
    let mut c = Command::new(bin);
    c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
        .env("VIBEKE_STATE_DIR", d.join("state"))
        .env("VIBEKE_CONFIG", d.join("config.toml"))
        .env("VIBEKE_NOTIFIER", format!("log:{}", d.join("notes.jsonl").display()))
        .env(
            "VIBEKE_CLIPBOARD_CMD",
            format!(
                "printf '%s:' \"$VIBEKE_CLIPBOARD_SELECTION\" >> '{0}'; cat >> '{0}'; echo >> '{0}'",
                clip.display()
            ),
        )
        .env("PS1", "$ ");
    // A local, untmuxed, non-iTerm2 host: OSC 52 plus the (hooked) platform tool.
    for k in [
        "VIBEKE",
        "VIBEKE_SOCKET",
        "VIBEKE_SESSION",
        "VIBEKE_PANE_TOKEN",
        "VIBEKE_PANE_ID",
        "SSH_CONNECTION",
        "SSH_TTY",
        "SSH_CLIENT",
        "TMUX",
        "LC_TERMINAL",
        "TERM_PROGRAM",
    ] {
        c.env_remove(k);
    }
    let keys = "{wait:$ }echo vk-sel-$((6*7)) tail-$((5+5))-word{enter}{wait:tail-10-word}\
                {drag:vk-sel-42}{sleep:300}{dclick:tail-10-word}{sleep:300}";
    let out = c
        .args([
            "debug", "ptyshot", "--cols", "100", "--rows", "30", "--settle", "2500", "--keys",
            keys, "--", bin, "attach",
        ])
        .output()
        .unwrap();
    let _ = Command::new(bin)
        .env("VIBEKE_RUNTIME_DIR", d.join("run"))
        .env("VIBEKE_STATE_DIR", d.join("state"))
        .env("VIBEKE_CONFIG", d.join("config.toml"))
        .args(["server", "stop", "--kill-panes"])
        .output();
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        !stderr.contains("not on screen"),
        "{stderr}\n--- screen ---\n{stdout}"
    );
    let sets: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("[host clipboard set: "))
        .collect();
    assert_eq!(
        sets,
        vec!["\"vk-sel-42\"]", "\"tail-10-word\"]"],
        "OSC 52 writes in the TUI's output\n{stderr}\n--- screen ---\n{stdout}"
    );
    // The local platform clipboard (hooked) got the same text.
    let log = std::fs::read_to_string(&clip).unwrap_or_default();
    assert_eq!(log, "clipboard:vk-sel-42\nclipboard:tail-10-word\n");
    // The copy toast was drawn.
    assert!(stdout.contains("copied 12 chars"), "{stdout}");
}
