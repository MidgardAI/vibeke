//! `vibeke screenshot add` end to end: an agent in a pane attaches an image file, the server
//! records it as an `agent` screenshot of that pane, and `screenshot list --environment agent`
//! finds it with the caption. Also the full-scope path (`--pane`) and a refused file.

use crate::support::Session;
use base64::Engine as _;
use std::time::{Duration, Instant};

/// A 1x1 PNG.
const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mNkYPhfDwAChwGA60e6kgAAAABJRU5ErkJggg==";

#[test]
fn agent_attaches_an_image_from_inside_a_pane() {
    let s = Session::new();
    let d = s.dir.path().to_path_buf();
    let file = d.join("login.png");
    std::fs::write(
        &file,
        base64::engine::general_purpose::STANDARD
            .decode(PNG_B64)
            .unwrap(),
    )
    .unwrap();
    let garbage = d.join("notes.txt");
    std::fs::write(&garbage, b"not an image").unwrap();

    let pane = s.workspace("/bin/sh");
    let _ = s
        .cmd(&[
            "pane",
            "wait-idle",
            &pane,
            "--quiet-ms",
            "500",
            "--timeout-ms",
            "10000",
        ])
        .output();
    // Inside the pane the CLI is pane-scoped (token from the environment).
    let out = d.join("out.txt");
    let done = d.join("done.txt");
    let line = format!(
        "$VIBEKE_BIN screenshot add {f} --caption 'Login page' > {o} 2>&1; \
         $VIBEKE_BIN screenshot add {f} --caption 'Login page' >> {o} 2>&1; \
         $VIBEKE_BIN screenshot add {g} >> {o} 2>&1; echo $? > {done}",
        f = file.display(),
        g = garbage.display(),
        o = out.display(),
        done = done.display(),
    );
    s.json(&["pane", "run", &pane, &line]);
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done.exists() {
        assert!(Instant::now() < deadline, "the pane command did not finish");
        std::thread::sleep(Duration::from_millis(100));
    }
    // Wait for the exit-status write to complete.
    std::thread::sleep(Duration::from_millis(100));
    let text = std::fs::read_to_string(&out).unwrap();
    // One JSON object per file with --json absent in the pane: text lines.
    assert!(text.contains("login.png"), "{text}");
    assert!(text.contains("(shown to the user)"), "{text}");
    assert!(text.contains("(already shown to the user)"), "{text}");
    assert!(text.contains("invalid_params"), "{text}");
    let status = std::fs::read_to_string(&done).unwrap();
    assert_ne!(status.trim(), "0", "a refused file must fail the command");

    let list = s.json(&["screenshot", "list", "--environment", "agent"]);
    assert_eq!(list["count"], 1, "{list}");
    let shot = &list["screenshots"][0];
    assert_eq!(shot["caption"], "Login page");
    assert_eq!(shot["source_name"], "login.png");
    assert_eq!(shot["pane"], pane.as_str());
    assert_eq!(shot["environment"]["kind"], "agent");
    assert_eq!(shot["taken_by"]["kind"], "agent");
    assert_eq!(shot["exists"], true);
    // The pane filter and an unrelated environment agree.
    let by_pane = s.json(&["screenshot", "list", "--pane", &pane]);
    assert_eq!(by_pane["count"], 1);
    let none = s.json(&["screenshot", "list", "--environment", "remote_headless"]);
    assert_eq!(none["count"], 0);
}

#[test]
fn user_attaches_an_image_to_a_named_pane() {
    let s = Session::new();
    let file = s.dir.path().join("shot.png");
    std::fs::write(
        &file,
        base64::engine::general_purpose::STANDARD
            .decode(PNG_B64)
            .unwrap(),
    )
    .unwrap();
    let pane = s.workspace("/bin/sh");
    let f = file.to_str().unwrap();
    let r = s.json(&[
        "screenshot",
        "add",
        f,
        "--caption",
        "From the user",
        "--pane",
        &pane,
    ]);
    assert_eq!(r["duplicate"], false, "{r}");
    assert_eq!(r["taken_by"]["kind"], "user");
    assert_eq!(r["pane"], pane.as_str());
    assert_eq!(r["file"], f);
    let list = s.json(&[
        "screenshot",
        "list",
        "--environment",
        "agent",
        "--pane",
        &pane,
    ]);
    assert_eq!(list["count"], 1);
    assert_eq!(list["screenshots"][0]["caption"], "From the user");
    // A file that does not exist fails without a server round trip.
    let out = s
        .cmd(&["screenshot", "add", "/nonexistent/none.png"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    // A file over 11 MiB is refused by the CLI before it is read or sent (a sparse file).
    let big = s.dir.path().join("big.png");
    std::fs::File::create(&big)
        .unwrap()
        .set_len((11 << 20) + 1)
        .unwrap();
    let out = s
        .cmd(&["screenshot", "add", big.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("image is larger than 11 MiB"), "{err}");
    let list = s.json(&["screenshot", "list", "--environment", "agent"]);
    assert_eq!(list["count"], 1, "{list}");
}
