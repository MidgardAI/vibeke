//! Real installation/restart, isolated from the user's installation. A local development
//! artifact explicitly opts into the unsigned test path; online updates never use that path.
use crate::support::{Session, alive};

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

#[test]
fn rollback_and_explicit_downgrade_refuse_incompatible_schema_before_switch_or_restart() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let s = Session::new();
    let pane = s.workspace("sleep 1000");
    let child = s.pane(&pane)["child_pid"].as_i64().unwrap();
    let before = s.json(&["server", "status"]);
    let data = s.dir.path().join("data/vibeke");
    let old_dir = data.join("versions/0.0.0");
    let current = format!("versions/{}", env!("CARGO_PKG_VERSION"));
    std::fs::create_dir_all(&old_dir).unwrap();
    std::fs::create_dir_all(data.join(&current)).unwrap();
    symlink(&current, data.join("current")).unwrap();
    std::fs::write(data.join("previous"), "0.0.0\n").unwrap();
    let candidate = old_dir.join("vibeke");
    std::fs::write(&candidate, "#!/bin/sh\ncase \"$1\" in\n--version) echo 'vibeke 0.0.0';;\n--internal-schema-version) echo 0;;\n*) touch \"$VIBEKE_TEST_MARKER\"; exit 1;;\nesac\n").unwrap();
    std::fs::set_permissions(&candidate, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sha = vk_remote::bootstrap::sha256_file(&candidate).unwrap();
    std::fs::write(
        candidate.with_extension("sha256"),
        format!("{sha}  vibeke\n"),
    )
    .unwrap();
    let marker = s.dir.path().join("unexpected-server-exec");
    for args in [
        vec!["--pretty", "update", "--rollback"],
        vec![
            "--pretty",
            "update",
            "--from",
            candidate.to_str().unwrap(),
            "--allow-downgrade",
        ],
    ] {
        let out = s
            .cmd(&args)
            .env("HOME", s.dir.path().join("home"))
            .env("XDG_DATA_HOME", s.dir.path().join("data"))
            .env("VIBEKE_ALLOW_UNSIGNED", "1")
            .env("VIBEKE_TEST_MARKER", &marker)
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("refusing downgrade"),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            std::fs::read_link(data.join("current")).unwrap(),
            std::path::PathBuf::from(&current)
        );
        assert_eq!(s.json(&["server", "status"])["boot_id"], before["boot_id"]);
        assert!(alive(child));
        assert!(!marker.exists());
    }
}
