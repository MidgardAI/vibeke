//! OSC 133 prompt marks end to end (03 §8, §11.1): the real TUI runs in a PTY under
//! `vibeke debug ptyshot` against an isolated session. A shell prints two shell-integrated
//! commands; in copy mode `[` jumps to the previous prompt, `o` selects that command's output
//! and `y` yanks it, so the TUI's output stream must carry an OSC 52 write of exactly the
//! output lines. The platform clipboard tool is replaced by `VIBEKE_CLIPBOARD_CMD`, so the host
//! clipboard and terminal are never touched.

use std::process::Command;

#[test]
fn copy_mode_prompt_jump_selects_the_last_command_output() {
    let dir = tempfile::Builder::new()
        .prefix("vkosc")
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
        .env(
            "VIBEKE_NOTIFIER",
            format!("log:{}", d.join("notes.jsonl").display()),
        )
        .env(
            "VIBEKE_CLIPBOARD_CMD",
            format!("cat >> '{0}'; echo >> '{0}'", clip.display()),
        )
        .env("PS1", "$ ");
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
    // Two shell-integrated commands (A prompt, B command line, C output, D exit), then an idle
    // prompt. `\055` is `-`, so the typed command line never contains the output text.
    let marks = r"printf '\033]133;A\007P1$ \033]133;B\007make\r\n\033]133;C\007first\055out\r\n\033]133;D;0\007\033]133;A\007P2$ \033]133;B\007test\r\n\033]133;C\007out\055one\r\nout\055two\r\n\033]133;D;1\007\033]133;A\007P3$ \033]133;B\007'";
    let keys = format!(
        "{{wait:$ }}{marks}{{enter}}{{wait:out-two}}{{sleep:300}}{{ctrl+b}}[{{sleep:400}}[{{sleep:200}}o{{sleep:200}}y{{sleep:600}}"
    );
    let out = c
        .args([
            "debug", "ptyshot", "--cols", "100", "--rows", "30", "--settle", "2000", "--keys",
            &keys, "--", bin, "attach",
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
        vec!["\"out-one\\nout-two\"]"],
        "OSC 52 writes in the TUI's output\n{stderr}\n--- screen ---\n{stdout}"
    );
    let log = std::fs::read_to_string(&clip).unwrap_or_default();
    assert_eq!(log, "out-one\nout-two\n");
}
