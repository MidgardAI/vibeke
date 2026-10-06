//! Lane 1E end to end: `vibeke machine show|connect|disconnect|upgrade`, `agent list
//! --all-machines` and `attach-file`, against a **fake machine**: a fake `ssh`
//! (`VIBEKE_SSH`) that runs the remote command on this host with a temporary `$HOME` and its
//! own runtime/state dirs, exactly where ssh would run it. No real host is ever contacted;
//! `upgrade` only ever touches the temporary home.

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Env {
    dir: tempfile::TempDir,
    ssh: PathBuf,
}

impl Env {
    fn new() -> Env {
        let dir = tempfile::Builder::new()
            .prefix("vkrm")
            .tempdir_in("/tmp")
            .unwrap();
        let d = dir.path();
        std::fs::write(
            d.join("config.toml"),
            "[[remote.machine]]\nlabel = \"fakebox\"\naddress = \"fakebox\"\n\n\
             [[remote.machine]]\nlabel = \"deadbox\"\naddress = \"deadbox\"\n",
        )
        .unwrap();
        // The fake machine: its home has vibeke "installed" as this test build.
        let home = d.join("remote-home");
        let cur = home.join(".local/share/vibeke/versions/test");
        std::fs::create_dir_all(&cur).unwrap();
        std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_vibeke"), cur.join("vibeke")).unwrap();
        std::os::unix::fs::symlink("versions/test", home.join(".local/share/vibeke/current"))
            .unwrap();
        let r = d.join("remote");
        std::fs::create_dir_all(&r).unwrap();
        std::fs::write(r.join("config.toml"), "").unwrap();
        let ssh = d.join("fake-ssh");
        std::fs::write(
            &ssh,
            format!(
                r#"#!/bin/sh
# Fake ssh: never contacts a host. `deadbox` is unreachable; `-O check|exit` answers like a
# ControlMaster; anything else runs the remote command here with the fake machine's HOME.
for a in "$@"; do
  case "$a" in deadbox) echo "ssh: connect to host deadbox: Connection refused" >&2; exit 255;; esac
done
op=""
prev=""
for a in "$@"; do
  if [ "$prev" = "-O" ]; then op="$a"; fi
  prev="$a"
done
case "$op" in
  check) echo "Master running (pid=1)"; exit 0;;
  exit) echo "Exit request sent."; exit 0;;
esac
last=""
for a in "$@"; do last="$a"; done
export HOME={home} VIBEKE_RUNTIME_DIR={r}/run VIBEKE_STATE_DIR={r}/state VIBEKE_CONFIG={r}/config.toml VIBEKE_SSH=
unset VIBEKE VIBEKE_SOCKET VIBEKE_SESSION VIBEKE_PANE_TOKEN VIBEKE_PANE_ID VIBEKE_ALLOW_UNSIGNED
exec sh -c "$last"
"#,
                home = home.display(),
                r = r.display(),
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
        Env { dir, ssh }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_SSH", &self.ssh)
            .env("HOME", d.join("local-home"));
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_PANE_ID",
            "VIBEKE_ALLOW_UNSIGNED",
            "VIBEKE_ARTIFACT_DIR",
        ] {
            c.env_remove(k);
        }
        c.args(args);
        c
    }

    fn run(&self, args: &[&str]) -> (bool, String, String) {
        let out = self.cmd(args).output().unwrap();
        (
            out.status.success(),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut a = vec!["--json"];
        a.extend_from_slice(args);
        let (ok, out, err) = self.run(&a);
        assert!(ok, "{args:?} failed: {err}\n{out}");
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{args:?}: {e}: {out}"))
    }

    fn remote_inbox(&self) -> PathBuf {
        self.path().join("remote/state/inbox")
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
        let _ = self
            .cmd(&["--machine", "fakebox", "server", "stop", "--kill-panes"])
            .output();
    }
}

fn remote_pane(e: &Env) -> String {
    let panes = |e: &Env| -> Vec<Value> {
        e.json(&["--machine", "fakebox", "pane", "list"])["panes"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    if panes(e).is_empty() {
        let cwd = e.path().display().to_string();
        e.json(&["--machine", "fakebox", "workspace", "create", "--cwd", &cwd]);
    }
    panes(e)[0]["id"].as_str().unwrap().to_string()
}

#[test]
fn machine_verbs_agent_list_and_attach_file_against_a_fake_machine() {
    let e = Env::new();

    // show: saved settings, the probed platform/version, a live link, the remote server.
    let v = e.json(&["machine", "show", "fakebox"]);
    assert_eq!(v["machine"], "fakebox");
    assert_eq!(v["saved"]["bootstrap"], "push");
    assert_eq!(v["remote"]["vibeke"], env!("CARGO_PKG_VERSION"), "{v:#}");
    assert_eq!(v["link"]["state"], "connected", "{v:#}");
    assert_eq!(v["server"]["version"], env!("CARGO_PKG_VERSION"), "{v:#}");
    // No release keys and no opt-in: no verified local artifact.
    assert_eq!(v["local_artifact"], Value::Null);
    // --offline never touches the machine.
    let off = e.json(&["machine", "show", "deadbox", "--offline"]);
    assert_eq!(off["machine"], "deadbox");
    assert!(off.get("remote").is_none());

    // connect / disconnect.
    let c = e.json(&["machine", "connect", "fakebox"]);
    assert_eq!(c["connected"], true);
    assert_eq!(c["server"]["version"], env!("CARGO_PKG_VERSION"));
    let (ok, _, err) = e.run(&["machine", "connect", "deadbox"]);
    assert!(!ok);
    assert!(err.contains("remote_unavailable"), "{err}");
    let d = e.json(&["machine", "disconnect", "fakebox"]);
    assert_eq!(d["disconnected"], true);

    // attach-file: the file lands in the remote inbox and its path is pasted into the pane.
    let pane = remote_pane(&e);
    let local = e.path().join("Screenshot 2026-10-05 at 20.49.03.png");
    std::fs::write(&local, b"\x89PNG fake image").unwrap();
    let target = format!("fakebox/{pane}");
    let a = e.json(&["attach-file", local.to_str().unwrap(), "--pane", &target]);
    let landed = PathBuf::from(a["path"].as_str().unwrap());
    assert!(landed.starts_with(e.remote_inbox()), "{landed:?}");
    assert!(landed.ends_with("Screenshot 2026-10-05 at 20.49.03.png"));
    assert_eq!(std::fs::read(&landed).unwrap(), b"\x89PNG fake image");
    // A directory is unpacked under the inbox.
    let proj = e.path().join("proj");
    std::fs::create_dir_all(proj.join("src")).unwrap();
    std::fs::write(proj.join("src/main.rs"), "fn main() {}").unwrap();
    let a = e.json(&[
        "attach-file",
        proj.to_str().unwrap(),
        "--pane",
        &target,
        "--no-paste",
    ]);
    let dir = PathBuf::from(a["path"].as_str().unwrap());
    assert_eq!(a["dir"], true);
    assert_eq!(
        std::fs::read_to_string(dir.join("src/main.rs")).unwrap(),
        "fn main() {}"
    );
    // paste.translated was recorded with basenames only.
    let ev = e.json(&[
        "--machine",
        "fakebox",
        "events",
        "read",
        "--types",
        "paste.translated",
    ]);
    let evs = ev["events"].as_array().unwrap();
    assert_eq!(evs.len(), 2, "{ev:#}");
    assert_eq!(
        evs[0]["data"]["files"][0]["local_name"],
        "Screenshot 2026-10-05 at 20.49.03.png"
    );
    assert_eq!(evs[0]["data"]["target_namespace"], "ssh:fakebox");
    assert_eq!(evs[1]["data"]["files"][0]["dir"], true);
    assert!(!ev.to_string().contains(&e.path().display().to_string()));
    // The pasted path reaches the pane (backslash-escaped, as a drop would be).
    let t0 = Instant::now();
    loop {
        let r = e.json(&["--machine", "fakebox", "pane", "read", &pane]);
        if r["text"]
            .as_str()
            .unwrap_or("")
            .contains("Screenshot\\ 2026")
        {
            break;
        }
        assert!(
            t0.elapsed() < Duration::from_secs(15),
            "pasted path not in the pane: {r:#}"
        );
        std::thread::sleep(Duration::from_millis(200));
    }

    // agent list --all-machines: local + fakebox answer, deadbox is listed offline.
    let l = e.json(&["agent", "list", "--all-machines"]);
    assert!(l["agents"].is_array(), "{l:#}");
    let off: Vec<&str> = l["offline"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["machine"].as_str().unwrap())
        .collect();
    assert_eq!(off, vec!["deadbox"], "{l:#}");
}

fn artifact(dir: &Path, version: &str, good_sum: bool) -> PathBuf {
    let p = dir.join("vibeke-linux-test");
    std::fs::write(&p, format!("#!/bin/sh\necho \"vibeke {version}\"\n")).unwrap();
    use sha2_shim::sha256_hex;
    let sum = if good_sum {
        sha256_hex(&std::fs::read(&p).unwrap())
    } else {
        "ab".repeat(32)
    };
    std::fs::write(
        dir.join("vibeke-linux-test.sha256"),
        format!("{sum}  vibeke-linux-test\n"),
    )
    .unwrap();
    p
}

/// `shasum` keeps this test free of a hashing dependency.
mod sha2_shim {
    pub fn sha256_hex(data: &[u8]) -> String {
        use std::io::Write;
        let mut c = std::process::Command::new("shasum")
            .args(["-a", "256"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        c.stdin.take().unwrap().write_all(data).unwrap();
        let out = c.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .next()
            .unwrap()
            .to_string()
    }
}

#[test]
fn upgrade_stages_verifies_and_switches_only_the_fake_home() {
    let e = Env::new();
    let home = e.path().join("remote-home/.local/share/vibeke");
    let arts = e.path().join("dist");
    std::fs::create_dir_all(&arts).unwrap();

    // A bad checksum is refused before anything is sent.
    let bad = artifact(&arts, "9.9.9", false);
    let mut c = e.cmd(&[
        "machine",
        "upgrade",
        "fakebox",
        "--from",
        bad.to_str().unwrap(),
        "--version",
        "9.9.9",
    ]);
    c.env("VIBEKE_ALLOW_UNSIGNED", "1");
    let out = c.output().unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("checksum mismatch"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!home.join("versions/9.9.9").exists());

    // Unsigned without the opt-in: refused (no release keys exist yet).
    let good = artifact(&arts, "9.9.9", true);
    let (ok, _, err) = e.run(&[
        "machine",
        "upgrade",
        "fakebox",
        "--from",
        good.to_str().unwrap(),
        "--version",
        "9.9.9",
    ]);
    assert!(!ok);
    assert!(err.contains("not signed"), "{err}");

    // --stage-only: staged and verified on the "remote", `current` untouched.
    let mut c = e.cmd(&[
        "machine",
        "upgrade",
        "fakebox",
        "--from",
        good.to_str().unwrap(),
        "--version",
        "9.9.9",
        "--stage-only",
    ]);
    c.env("VIBEKE_ALLOW_UNSIGNED", "1");
    let out = c.output().unwrap();
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{so}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(so.contains("staged vibeke 9.9.9"), "{so}");
    assert!(home.join("versions/9.9.9/vibeke").is_file());
    assert!(!home.join("versions/9.9.9/vibeke.tmp").exists());
    assert_eq!(
        std::fs::read_link(home.join("current")).unwrap(),
        PathBuf::from("versions/test")
    );

    // Full upgrade: switches `current` in the fake home only.
    let mut c = e.cmd(&[
        "machine",
        "upgrade",
        "fakebox",
        "--from",
        good.to_str().unwrap(),
        "--version",
        "9.9.9",
    ]);
    c.env("VIBEKE_ALLOW_UNSIGNED", "1");
    let out = c.output().unwrap();
    let so = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{so}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(so.contains("upgraded"), "{so}");
    assert_eq!(
        std::fs::read_link(home.join("current")).unwrap(),
        PathBuf::from("versions/9.9.9")
    );
    let _ = json!(null);
}
