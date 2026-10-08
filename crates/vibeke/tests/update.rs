//! Real installation/restart, isolated from the user's installation. A local development
//! artifact explicitly opts into the unsigned test path; online updates never use that path.
mod support;
use support::{Session, alive};

#[test]
fn update_restarts_the_server_without_replacing_pane_processes() {
    let s = Session::new();
    let pane = s.workspace("sleep 1000");
    let child = s.pane(&pane)["child_pid"].as_i64().unwrap();
    let before = s.json(&["server", "status"]);
    let candidate = s.dir.path().join("candidate");
    std::fs::copy(env!("CARGO_BIN_EXE_vibeke"), &candidate).unwrap();
    let sha = vk_remote::bootstrap::sha256_file(&candidate).unwrap();
    std::fs::write(
        candidate.with_extension("sha256"),
        format!("{sha}  candidate\n"),
    )
    .unwrap();
    let home = s.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = s
        .cmd(&["--pretty", "update", "--from", candidate.to_str().unwrap()])
        .env("HOME", &home)
        .env("XDG_DATA_HOME", s.dir.path().join("data"))
        .env("VIBEKE_ALLOW_UNSIGNED", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let after = s.json(&["server", "status"]);
    assert_eq!(
        after["pid"], before["pid"],
        "server exec preserves its PID and options"
    );
    assert_ne!(after["boot_id"], before["boot_id"]);
    assert_eq!(after["version"], env!("CARGO_PKG_VERSION"));
    assert!(alive(child), "holder keeps the running process alive");
    assert_eq!(s.pane(&pane)["child_pid"].as_i64(), Some(child));
    assert!(home.join(".local/bin/vibeke").exists());
}

#[test]
fn failed_verification_does_not_install_or_restart() {
    let s = Session::new();
    s.workspace("sleep 1000");
    let before = s.json(&["server", "status"]);
    let candidate = s.dir.path().join("candidate");
    std::fs::copy(env!("CARGO_BIN_EXE_vibeke"), &candidate).unwrap();
    std::fs::write(
        candidate.with_extension("sha256"),
        format!("{}  candidate\n", "0".repeat(64)),
    )
    .unwrap();
    let home = s.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let out = s
        .cmd(&["--pretty", "update", "--from", candidate.to_str().unwrap()])
        .env("HOME", &home)
        .env("XDG_DATA_HOME", s.dir.path().join("data"))
        .env_remove("VIBEKE_ALLOW_UNSIGNED")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(s.json(&["server", "status"])["boot_id"], before["boot_id"]);
    assert!(!home.join(".local/bin/vibeke").exists());
}
