//! Restricted legacy mode for plugins under the real macOS sandbox (`sandbox-exec`): the
//! plugin dir is read-only, its own state dir writable, other plugins' state and the user's
//! secrets are invisible, and the network is off unless granted. Temp dirs only.
#![cfg(target_os = "macos")]

use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};
use vk_sandbox::plugin::{self, PluginBox};

struct Fx {
    _t: tempfile::TempDir,
    root: PathBuf,
    b: PluginBox,
}

fn fixture() -> Fx {
    let t = tempfile::Builder::new()
        .prefix("vkpr")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().canonicalize().unwrap();
    for d in [
        "h/.ssh",
        "s/plugins/checkouts/a.b/bin",
        "s/plugins/state/a.b",
        "s/plugins/state/other",
        "c/plugins/a.b",
        "s/prof",
    ] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    std::fs::write(root.join("h/.ssh/id"), "SECRET").unwrap();
    std::fs::write(root.join("s/plugins/state/other/data"), "OTHER").unwrap();
    std::fs::write(root.join("s/plugins/checkouts/a.b/herdr-plugin.toml"), "x").unwrap();
    std::fs::write(root.join("c/plugins/a.b/cfg"), "cfg").unwrap();
    let b = PluginBox {
        home: root.join("h"),
        plugin_root: root.join("s/plugins/checkouts/a.b"),
        config_dir: root.join("c/plugins/a.b"),
        state_dir: root.join("s/plugins/state/a.b"),
        hidden: vec![root.join("s"), root.join("c")],
        extra_read: vec![],
        sockets: vec![],
        network: false,
        vibeke_bin: None,
        profile_dir: root.join("s/prof"),
    };
    Fx { _t: t, root, b }
}

fn run(b: &PluginBox, script: &str) -> Output {
    let argv: Vec<String> = ["/bin/sh", "-c", script]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let inv_env = plugin::scrubbed_env(vec![
        ("PATH".to_string(), "/usr/bin:/bin".to_string()),
        ("HOME".to_string(), b.home.to_string_lossy().into_owned()),
        ("AWS_SECRET_ACCESS_KEY".to_string(), "leak".to_string()),
    ]);
    let p = b.prepare(&argv, &b.plugin_root, inv_env, "t1").unwrap();
    assert!(p.profile.starts_with(&b.profile_dir));
    Command::new(&p.argv[0])
        .args(&p.argv[1..])
        .current_dir(&b.plugin_root)
        .env_clear()
        .envs(p.env.iter().cloned())
        .output()
        .unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn files_are_confined_to_the_plugins_own_dirs() {
    if plugin::probe().is_err() {
        eprintln!("skipped: no working sandbox here (nested?)");
        return;
    }
    let fx = fixture();
    let o = run(
        &fx.b,
        r#"
        echo state > "$PWD/../../state/a.b/out" 2>/dev/null; echo "state_write=$?"
        echo x > herdr-plugin.toml 2>/dev/null; echo "root_write=$?"
        echo x > "$PWD/newfile" 2>/dev/null; echo "root_create=$?"
        cat herdr-plugin.toml >/dev/null 2>&1; echo "root_read=$?"
        cat "$HOME/.ssh/id" >/dev/null 2>&1; echo "ssh_read=$?"
        cat "$PWD/../../state/other/data" >/dev/null 2>&1; echo "other_state_read=$?"
        echo y > "$PWD/../../state/other/data" 2>/dev/null; echo "other_state_write=$?"
        cat "$PWD/../../../../c/plugins/a.b/cfg" >/dev/null 2>&1; echo "cfg_read=$?"
        echo z > "$PWD/../../../../c/plugins/a.b/cfg" 2>/dev/null; echo "cfg_write=$?"
        echo t > "$TMPDIR/t" 2>/dev/null; echo "tmp_write=$?"
        echo "aws=${AWS_SECRET_ACCESS_KEY:-unset}"
        "#,
    );
    let t = text(&o);
    let get = |k: &str| -> String {
        t.lines()
            .find_map(|l| l.strip_prefix(&format!("{k}=")))
            .unwrap_or("missing")
            .to_string()
    };
    assert_eq!(get("state_write"), "0", "{t}");
    assert_eq!(
        std::fs::read_to_string(fx.root.join("s/plugins/state/a.b/out")).unwrap_or_default(),
        "state\n"
    );
    assert_ne!(get("root_write"), "0", "plugin dir must be read-only\n{t}");
    assert_ne!(get("root_create"), "0", "{t}");
    assert_eq!(get("root_read"), "0", "plugin dir stays readable\n{t}");
    assert_ne!(get("ssh_read"), "0", "{t}");
    assert_ne!(
        get("other_state_read"),
        "0",
        "other plugins' state hidden\n{t}"
    );
    assert_ne!(get("other_state_write"), "0", "{t}");
    assert_eq!(get("cfg_read"), "0", "{t}");
    assert_ne!(get("cfg_write"), "0", "config is read-only\n{t}");
    assert_eq!(get("tmp_write"), "0", "{t}");
    assert_eq!(get("aws"), "unset", "{t}");
    assert_eq!(
        std::fs::read_to_string(fx.root.join("s/plugins/state/other/data")).unwrap(),
        "OTHER"
    );
}

#[test]
fn network_is_off_unless_granted() {
    if plugin::probe().is_err() {
        eprintln!("skipped: no working sandbox here (nested?)");
        return;
    }
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || while l.accept().is_ok() {});
    let mut fx = fixture();
    // `nc -z` connects without sending anything.
    let probe = format!("/usr/bin/nc -z -w 2 127.0.0.1 {port} >/dev/null 2>&1; echo \"nc=$?\"");
    let off = text(&run(&fx.b, &probe));
    assert!(!off.contains("nc=0"), "network must be off\n{off}");
    fx.b.network = true;
    let on = text(&run(&fx.b, &probe));
    assert!(on.contains("nc=0"), "network granted\n{on}");
}

/// `network = true` opens remote IP endpoints only: the Unix-socket allowlist (the invocation's
/// own broker) stays, so another invocation's broker is neither connectable nor discoverable
/// (the runtime dir with `brokers.json` stays hidden).
#[test]
fn open_network_keeps_unix_sockets_and_the_runtime_dir_closed() {
    if plugin::probe().is_err() {
        eprintln!("skipped: no working sandbox here (nested?)");
        return;
    }
    let mut fx = fixture();
    let run_dir = fx.root.join("r/default/herdr-compat");
    std::fs::create_dir_all(run_dir.join("brokers")).unwrap();
    let own = run_dir.join("brokers/own.sock");
    let other = run_dir.join("brokers/other.sock");
    std::fs::write(
        run_dir.join("brokers.json"),
        format!("[{{\"path\": \"{}\"}}]", other.display()),
    )
    .unwrap();
    for p in [&own, &other] {
        let l = std::os::unix::net::UnixListener::bind(p).unwrap();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                use std::io::{Read, Write};
                let mut s = s;
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ = s.write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
            }
        });
    }
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = tcp.local_addr().unwrap().port();
    std::thread::spawn(move || while tcp.accept().is_ok() {});
    fx.b.hidden.push(fx.root.join("r"));
    fx.b.sockets = vec![own.clone()];
    fx.b.network = true;
    // curl exits 7 when it cannot connect, 0 when the socket answered.
    let script = format!(
        r#"
        /usr/bin/curl -s -m 3 --unix-socket "{own}" http://x/ >/dev/null 2>&1; echo "own=$?"
        /usr/bin/curl -s -m 3 --unix-socket "{other}" http://x/ >/dev/null 2>&1; echo "other=$?"
        cat "{reg}" >/dev/null 2>&1; echo "registry=$?"
        ls "{dir}" >/dev/null 2>&1; echo "listing=$?"
        /usr/bin/nc -z -w 2 127.0.0.1 {port} >/dev/null 2>&1; echo "tcp=$?"
        "#,
        own = own.display(),
        other = other.display(),
        reg = run_dir.join("brokers.json").display(),
        dir = run_dir.join("brokers").display(),
    );
    let t = text(&run(&fx.b, &script));
    let get = |k: &str| -> String {
        t.lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{k}=")))
            .unwrap_or("missing")
            .to_string()
    };
    assert_eq!(get("tcp"), "0", "remote IP endpoints are open\n{t}");
    assert_eq!(get("own"), "0", "its own broker stays reachable\n{t}");
    assert_eq!(get("other"), "7", "another broker is not connectable\n{t}");
    assert_ne!(get("registry"), "0", "brokers.json is hidden\n{t}");
    assert_ne!(get("listing"), "0", "the broker dir cannot be listed\n{t}");
}
