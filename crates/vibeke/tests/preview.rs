//! Goal 03 Stage 1 end to end: preview discovery in real panes (06 B2), and the SOCKS route
//! (B3.1/B3.4) through a real `vibeke bridge` reached via a fake `ssh` (`VIBEKE_SSH`) that
//! carries the bridge's stdio exactly as ssh would — no remote machine involved.
//!
//! `VIBEKE_BROWSER_TESTS=1` additionally drives Playwright's Chromium (headless, temp profile)
//! through the route. The user's real browser and profiles are never used.

use serde_json::{Value, json};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
    extra_env: Vec<(String, String)>,
}

impl Session {
    fn new(config: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkprev")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Session {
            dir,
            extra_env: vec![],
        }
    }
    fn path(&self) -> &Path {
        self.dir.path()
    }
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"));
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_PANE_ID",
        ] {
            c.env_remove(k);
        }
        for (k, v) in &self.extra_env {
            c.env(k, v);
        }
        c.arg("--json").args(args);
        c
    }
    fn try_json(&self, args: &[&str]) -> Result<Value, String> {
        let out = self.cmd(args).output().unwrap();
        if !out.status.success() {
            return Err(String::from_utf8_lossy(&out.stderr).into_owned());
        }
        Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
    }
    fn json(&self, args: &[&str]) -> Value {
        self.try_json(args)
            .unwrap_or_else(|e| panic!("{args:?}: {e}\n{}", self.log_tail()))
    }
    fn log_tail(&self) -> String {
        let log = std::fs::read_to_string(self.path().join("state/default/logs/server.log"))
            .unwrap_or_default();
        log.lines()
            .rev()
            .take(30)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn previews(&self) -> Vec<Value> {
        self.json(&["preview", "list", "--all"])["previews"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
    fn wait_preview(&self, what: &str, f: impl Fn(&Value) -> bool) -> Value {
        let t0 = Instant::now();
        loop {
            if let Some(p) = self.previews().into_iter().find(|p| f(p)) {
                return p;
            }
            if t0.elapsed() > Duration::from_secs(15) {
                panic!(
                    "timed out waiting for {what}; previews: {:#?}\n{}",
                    self.previews(),
                    self.log_tail()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

/// A tiny HTTP server in this process; records request lines.
struct Http {
    port: u16,
    seen: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
}

fn http_server(body: &'static str) -> Http {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            let seen = seen2.clone();
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                let mut got = 0;
                let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                while got < buf.len() {
                    match s.read(&mut buf[got..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got += n,
                    }
                    if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf[..got]).into_owned();
                if let Some(first) = req.lines().next() {
                    seen.lock().unwrap().push(first.to_string());
                }
                let html = format!("<html><body><p id=m>{body}</p></body></html>");
                let _ = write!(
                    s,
                    "HTTP/1.0 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{html}",
                    html.len()
                );
            });
        }
    });
    Http { port, seen }
}

fn wait_idle(s: &Session, pane: &str) {
    let _ = s
        .cmd(&[
            "pane",
            "wait-idle",
            pane,
            "--quiet-ms",
            "600",
            "--timeout-ms",
            "10000",
        ])
        .output();
}

#[test]
fn discovery_lifecycle_and_declare() {
    if Command::new("python3").arg("--version").output().is_err() {
        eprintln!("python3 not found; skipping");
        return;
    }
    let s = Session::new("");
    let pane = s.json(&["workspace", "create", "--cwd", "/tmp"])["root_pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_idle(&s, &pane);
    // A real listener in the pane's process tree; port 0 → parse the printed port.
    // (`python3 -m http.server` would do, but its getfqdn() can stall for many seconds.)
    s.json(&[
        "pane",
        "run",
        &pane,
        "python3 -u -c \"import http.server as h, socketserver as ss; srv = ss.TCPServer(('127.0.0.1', 0), h.SimpleHTTPRequestHandler); print('listening on port', srv.server_address[1]); srv.serve_forever()\"",
    ]);
    let out = s.json(&[
        "pane",
        "wait-output",
        &pane,
        "--regex",
        r"listening on port \d+",
        "--timeout-ms",
        "15000",
    ]);
    let _ = out;
    let text = s.json(&["pane", "read", &pane, "--source", "recent", "--lines", "20"])["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let port: u64 = regex_port(&text).unwrap_or_else(|| panic!("no port in {text:?}"));
    let t0 = Instant::now();
    let p = s.wait_preview("suggestion with pid", |p| {
        p["port"] == port && p["pid"].is_u64()
    });
    assert!(
        t0.elapsed() < Duration::from_secs(5),
        "suggestion took {:?}",
        t0.elapsed()
    );
    assert_eq!(p["status"], "suggested");
    assert!(
        p["source"] == "listener" || p["source"] == "output_url",
        "{p}"
    );
    assert_eq!(p["pane"], json!(pane));
    let handle = p["handle"].as_str().unwrap().to_string();
    // Suggestions are hidden from the default list.
    let default_list = s.json(&["preview", "list"])["previews"].clone();
    assert!(
        !default_list
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["port"] == port),
        "{default_list}"
    );
    // Declaring the port confirms the suggestion.
    let d = s.json(&[
        "preview",
        "declare",
        &port.to_string(),
        "--label",
        "web",
        "--path",
        "/docs",
    ]);
    assert_eq!(d["preview"]["handle"], json!(handle));
    assert_eq!(d["preview"]["status"], "up");
    assert_eq!(d["preview"]["source"], "declared");
    assert_eq!(
        d["preview"]["url"],
        json!(format!("http://localhost:{port}/docs"))
    );
    // Stop the server: up → down (declared previews stay, down).
    let _ = s.cmd(&["pane", "send-keys", &pane, "ctrl+c"]).output();
    s.wait_preview("down", |p| p["port"] == port && p["status"] == "down");
    // Pane mode (Stage 2): a browser pane next to the preview's pane.
    let o = s.json(&["preview", "open", &handle]);
    assert_eq!(o["opened_in"], "pane", "{o}");
    assert_eq!(o["source_pane"], json!(pane));
    // Forget removes it.
    s.json(&["preview", "forget", &handle]);
    assert!(!s.previews().iter().any(|p| p["port"] == port));

    // A Vite banner printed in the pane for a port that is listening (here: in this process,
    // so only the output path can find it) → a labelled suggestion with the banner's path.
    let vite = http_server("vite");
    s.json(&[
        "pane",
        "run",
        &pane,
        &format!(
            "printf '  \\342\\236\\234  Local:   \\033[36mhttp://localhost:\\033[1m{}\\033[22m/app/\\033[39m\\n'",
            vite.port
        ),
    ]);
    let v = s.wait_preview("banner suggestion", |p| p["port"] == vite.port);
    assert_eq!(v["source"], "banner");
    assert_eq!(v["label"], "vite");
    assert_eq!(v["path"], "/app/");
    assert_eq!(
        v["url"],
        json!(format!("http://localhost:{}/app/", vite.port))
    );
    assert_eq!(v["status"], "suggested");
    // Events were recorded.
    let ev = s.json(&["api", "call", "events.read", r#"{"types":["preview.*"]}"#]);
    let kinds: Vec<String> = ev["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap_or("").to_string())
        .collect();
    for k in [
        "preview.discovered",
        "preview.declared",
        "preview.down",
        "preview.gone",
    ] {
        assert!(kinds.iter().any(|x| x == k), "{k} missing from {kinds:?}");
    }
}

fn regex_port(text: &str) -> Option<u64> {
    const P: &str = "listening on port ";
    let i = text.rfind(P)?;
    text[i + P.len()..]
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// The fake remote: an `ssh` replacement that runs `vibeke bridge` with its own dirs.
fn fake_ssh(dir: &Path) -> PathBuf {
    let remote = dir.join("remote");
    std::fs::create_dir_all(&remote).unwrap();
    let script = dir.join("fake-ssh");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\n# ignores ssh options/host; carries the bridge's stdio like ssh -T\nexec env VIBEKE_RUNTIME_DIR={r}/run VIBEKE_STATE_DIR={r}/state VIBEKE_CONFIG={r}/config.toml VIBEKE_SSH= {bin} bridge --session default\n",
            r = remote.display(),
            bin = env!("CARGO_BIN_EXE_vibeke"),
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

fn socks_get(socks: u16, host: &str, port: u16, path: &str) -> Result<String, u8> {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", socks)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    s.write_all(&[5, 1, 0]).unwrap();
    let mut m = [0u8; 2];
    s.read_exact(&mut m).unwrap();
    assert_eq!(m, [5, 0]);
    let mut req = vec![5, 1, 0, 3, host.len() as u8];
    req.extend_from_slice(host.as_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    s.write_all(&req).unwrap();
    let mut r = [0u8; 10];
    s.read_exact(&mut r).unwrap();
    if r[1] != 0 {
        return Err(r[1]);
    }
    write!(s, "GET {path} HTTP/1.0\r\nHost: {host}:{port}\r\n\r\n").unwrap();
    let mut out = String::new();
    let _ = s.read_to_string(&mut out);
    Ok(out)
}

#[test]
fn socks_route_through_bridge() {
    let mut s = Session::new("[[remote.machine]]\nlabel = \"fakebox\"\naddress = \"fakebox\"\n");
    let ssh = fake_ssh(s.path());
    s.extra_env = vec![
        ("VIBEKE_SSH".into(), ssh.to_string_lossy().into_owned()),
        ("VIBEKE_TEST_HOOKS".into(), "1".into()),
    ];
    // The dev server "on fakebox" (same host; reached only through the bridge's tcp: channel).
    let app = http_server("hello-through-the-bridge");
    s.json(&["server", "status"]);

    // A managed "browser" that is not us: the peer check must reject this process and curl.
    let mut stranger = Command::new("sleep").arg("60").spawn().unwrap();
    let r = s.json(&[
        "api",
        "call",
        "preview.test.register_browser",
        &json!({"pid": stranger.id(), "profile": "fakebox-a", "machine": "fakebox"}).to_string(),
    ]);
    let socks = r["socks_port"].as_u64().unwrap() as u16;
    assert_eq!(
        s.json(&["server", "status"])["preview"]["socks_port"],
        json!(socks)
    );
    assert_eq!(
        socks_get(socks, "localhost", app.port, "/"),
        Err(2),
        "unmanaged process must get reply 0x02"
    );
    if let Ok(out) = Command::new("curl")
        .args([
            "-s",
            "-m",
            "5",
            "--socks5-hostname",
            &format!("127.0.0.1:{socks}"),
            &format!("http://localhost:{}/curl", app.port),
        ])
        .output()
    {
        assert!(!out.status.success(), "curl got through: {out:?}");
    }
    assert!(
        app.seen.lock().unwrap().is_empty(),
        "nothing reached the app"
    );
    let st = s.json(&["preview", "status"]);
    assert!(st["rejected"].as_u64().unwrap() >= 1, "{st}");

    // Now this test process is the managed browser for a fakebox profile.
    s.json(&[
        "api",
        "call",
        "preview.test.register_browser",
        &json!({"pid": std::process::id(), "profile": "self-test", "machine": "fakebox"})
            .to_string(),
    ]);
    let body = socks_get(socks, "localhost", app.port, "/via-socks")
        .unwrap_or_else(|code| panic!("reply {code}\n{}", s.log_tail()));
    assert!(body.contains("hello-through-the-bridge"), "{body}");
    assert!(body.starts_with("HTTP/1.0 200"), "{body}");
    assert!(
        socks_get(socks, "127.0.0.1", app.port, "/v4")
            .unwrap()
            .contains("hello")
    );
    // Closed port on the remote → connection refused reply, not a hang.
    let closed = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    assert_eq!(socks_get(socks, "localhost", closed, "/"), Err(5));
    // The traffic crossed the fakebox link (bytes counted on the server's mux).
    let st = s.json(&["preview", "status"]);
    let link = st["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["machine"] == "fakebox")
        .cloned()
        .unwrap_or_else(|| panic!("no fakebox link: {st}"));
    assert!(link["bytes_in"].as_u64().unwrap() > 100, "{link}");
    let seen = app.seen.lock().unwrap().clone();
    assert!(
        seen.iter().any(|l| l.starts_with("GET /via-socks")),
        "{seen:?}"
    );

    // Remote preview lookup over the server's link: declare on fakebox via `--machine`
    // (forwarded by the CLI over its own fake-ssh link), then resolve it from the local server.
    let d = s.json(&[
        "--machine",
        "fakebox",
        "preview",
        "declare",
        &app.port.to_string(),
        "--label",
        "remote-web",
    ]);
    let rh = d["preview"]["handle"].as_str().unwrap().to_string();
    let got = s.json(&[
        "api",
        "call",
        "preview.get",
        &json!({"preview": format!("fakebox/{rh}")}).to_string(),
    ]);
    assert_eq!(got["preview"]["label"], "remote-web");
    let url = s.json(&["preview", "url", &format!("fakebox/{rh}")]);
    assert_eq!(
        url["profile_url"],
        json!(format!("http://localhost:{}/", app.port))
    );
    let listed = s.json(&["--machine", "fakebox", "preview", "list"]);
    assert!(
        listed["previews"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["handle"] == json!(rh))
    );

    if std::env::var("VIBEKE_BROWSER_TESTS").is_ok_and(|v| v == "1") {
        browser_checks(&s, socks, &app, &rh);
    }

    let _ = stranger.kill();
    let _ = stranger.wait();
    let _ = s
        .cmd(&["--machine", "fakebox", "server", "stop", "--kill-panes"])
        .output();
}

/// Playwright Chromium (on disk already; never downloaded, never the user's browser).
fn browser_checks(s: &Session, socks: u16, app: &Http, remote_handle: &str) {
    let Some(chromium) = playwright_chromium() else {
        eprintln!("no Playwright Chromium on disk; skipping browser checks");
        return;
    };
    // 1. Chromium as a child of this (managed) process, through the proxy, temp profile.
    let profile = tempfile::tempdir().unwrap();
    let mut child = Command::new(&chromium)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .args([
            "--headless=new",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-background-networking",
            &format!("--user-data-dir={}", profile.path().display()),
            &format!("--proxy-server=socks5://127.0.0.1:{socks}"),
            "--proxy-bypass-list=<-loopback>",
            &format!("http://localhost:{}/chromium", app.port),
        ])
        .spawn()
        .unwrap();
    // `localhost` must go through the proxy (`<-loopback>`), pass the peer check (Chromium's
    // network service is in this process's tree) and cross the bridge to the app.
    let t0 = Instant::now();
    while !app
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.starts_with("GET /chromium "))
    {
        if t0.elapsed() > Duration::from_secs(30) {
            let _ = child.kill();
            panic!(
                "chromium never fetched through the route; app saw {:?}\nstatus: {}\n{}",
                app.seen.lock().unwrap(),
                s.json(&["preview", "status"]),
                s.log_tail()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.kill();
    let _ = child.wait();
    // 2. The server's own launcher: `preview open fakebox/vN --window` (headless via the test
    // hook) on a Vibeke profile under the test state dir, peer-checked by its process tree.
    let mut cfg = std::fs::read_to_string(s.path().join("config.toml")).unwrap();
    cfg.push_str(&format!(
        "\n[preview]\nbrowser = \"{}\"\n",
        chromium.display()
    ));
    std::fs::write(s.path().join("config.toml"), cfg).unwrap();
    let r = s.json(&[
        "preview",
        "open",
        &format!("fakebox/{remote_handle}"),
        "--window",
        "--headless",
    ]);
    assert_eq!(r["opened_in"], "window");
    assert_eq!(r["profile"], "fakebox");
    let dir = r["profile_dir"].as_str().unwrap();
    assert!(dir.starts_with(s.path().to_str().unwrap()), "{dir}");
    assert_eq!(r["reused"], false);
    assert_eq!(r["route"], "loopback");
    let pid = r["pid"].as_u64().unwrap() as i32;
    let t0 = Instant::now();
    while !app
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.starts_with("GET / "))
    {
        if t0.elapsed() > Duration::from_secs(30) {
            // SAFETY: plain kill of the browser the server launched for this test.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            panic!(
                "launched browser never fetched: {:?}\nstatus: {}\n{}",
                app.seen.lock().unwrap(),
                s.json(&["preview", "status"]),
                s.log_tail()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let profiles = s.json(&["preview", "profile", "list"]);
    assert!(
        profiles["profiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "fakebox" && p["running"] == true),
        "{profiles}"
    );
    let e = s
        .try_json(&["preview", "profile", "reset", "fakebox"])
        .unwrap_err();
    assert!(e.contains("conflict"), "{e}");
    // SAFETY: plain kill of the browser the server launched for this test.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    // Reset refuses while running, works after.
    let t0 = Instant::now();
    loop {
        match s.try_json(&["preview", "profile", "reset", "fakebox"]) {
            Ok(v) => {
                assert_eq!(v["removed"], true);
                break;
            }
            Err(e) => {
                assert!(e.contains("conflict"), "{e}");
                assert!(t0.elapsed() < Duration::from_secs(10), "{e}");
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }
}

fn playwright_chromium() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let root = PathBuf::from(home).join(if cfg!(target_os = "macos") {
        "Library/Caches/ms-playwright"
    } else {
        ".cache/ms-playwright"
    });
    let mut dirs: Vec<(u32, PathBuf)> = std::fs::read_dir(&root)
        .ok()?
        .flatten()
        .filter_map(|e| {
            let n = e.file_name().to_string_lossy().into_owned();
            n.strip_prefix("chromium-")
                .and_then(|r| r.parse().ok())
                .map(|r| (r, e.path()))
        })
        .collect();
    dirs.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (_, d) in dirs {
        for rel in [
            "chrome-mac-arm64/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
            "chrome-mac/Chromium.app/Contents/MacOS/Chromium",
            "chrome-linux64/chrome",
            "chrome-linux/chrome",
        ] {
            let p = d.join(rel);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// A blocking HTTP/1.1 request to the preview proxy on 127.0.0.1 → (status, head, body).
fn proxy_get(port: u16, host: &str, path: &str, extra: &str) -> (u16, String, String) {
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    write!(
        s,
        "GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n"
    )
    .unwrap();
    let mut out = Vec::new();
    let _ = s.read_to_end(&mut out);
    let t = String::from_utf8_lossy(&out).into_owned();
    let (head, body) = t.split_once("\r\n\r\n").unwrap_or((&t, ""));
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|x| x.parse().ok())
        .unwrap_or(0);
    (status, head.to_string(), body.to_string())
}

/// B4 end to end with the real binary: a preview declared on the fake remote (a real
/// `vibeke bridge` behind a fake `ssh`) opened in proxy mode; the one-time link sets the
/// cookie, the app is reached over the bridge `tcp:` channel with the credential stripped;
/// no credential / forged host are refused. Mirror mode is off until asked for, and asking
/// for it while the port is busy locally is a conflict.
#[test]
fn proxy_mode_and_mirror_through_bridge() {
    let mut s = Session::new(
        "[[remote.machine]]\nlabel = \"fakebox\"\naddress = \"fakebox\"\n\n[preview]\nproxy_port = 0\n",
    );
    let ssh = fake_ssh(s.path());
    s.extra_env = vec![
        ("VIBEKE_SSH".into(), ssh.to_string_lossy().into_owned()),
        ("VIBEKE_NO_OPEN".into(), "1".into()),
    ];
    let app = http_server("hello-through-the-proxy");
    s.json(&["server", "status"]);
    let d = s.json(&[
        "--machine",
        "fakebox",
        "preview",
        "declare",
        &app.port.to_string(),
        "--label",
        "web",
        "--path",
        "/dash",
    ]);
    let rh = d["preview"]["handle"].as_str().unwrap().to_string();
    let target = format!("fakebox/{rh}");
    // Mirror mode is never on unless explicitly enabled.
    assert_eq!(s.json(&["preview", "status"])["mirrors"], json!([]));

    let r = s.json(&["preview", "open", &target, "--proxy", "--no-open"]);
    assert_eq!(r["opened_in"], "proxy", "{r}");
    assert_eq!(r["opened"], false);
    assert_eq!(r["machine"], "fakebox");
    let host = r["host"].as_str().unwrap().to_string();
    assert!(
        host.starts_with(&format!("{rh}-fakebox")) && host.ends_with(".vibeke.localhost"),
        "{host}"
    );
    let port = r["proxy_port"].as_u64().unwrap() as u16;
    let authority = format!("{host}:{port}");
    let open_url = r["open_url"].as_str().unwrap().to_string();
    let path = open_url.split_once(&authority).unwrap().1.to_string();
    assert!(path.starts_with("/dash?vk_token="), "{path}");

    // No credential, forged Host: refused, nothing reaches the app.
    assert_eq!(proxy_get(port, &authority, "/dash", "").0, 401);
    assert_eq!(
        proxy_get(port, &format!("evil.vibeke.localhost:{port}"), "/", "").0,
        421
    );
    assert_eq!(
        proxy_get(port, &format!("127.0.0.1:{port}"), "/", "").0,
        421
    );
    if let Ok(out) = Command::new("curl")
        .args([
            "-s",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "-m",
            "5",
            "--resolve",
            &format!("{host}:{port}:127.0.0.1"),
            &format!("http://{authority}/dash"),
        ])
        .output()
    {
        assert_eq!(String::from_utf8_lossy(&out.stdout), "401", "{out:?}");
    }
    assert!(
        app.seen.lock().unwrap().is_empty(),
        "nothing reached the app"
    );

    // The one-time link → 303 + host-only HttpOnly SameSite=Strict cookie.
    let (st, head, _) = proxy_get(port, &authority, &path, "");
    assert_eq!(st, 303, "{head}");
    let lower = head.to_ascii_lowercase();
    assert!(lower.contains("location: /dash\r\n"), "{head}");
    let cookie = lower
        .lines()
        .find_map(|l| l.strip_prefix("set-cookie: __host-vk_preview="))
        .unwrap_or_else(|| panic!("{head}"))
        .split(';')
        .next()
        .unwrap()
        .to_string();
    assert!(lower.contains("httponly") && lower.contains("samesite=strict"));
    // Replay of the link: refused.
    assert_eq!(proxy_get(port, &authority, &path, "").0, 401);
    // With the cookie: the app on "fakebox", over the bridge.
    let ck = format!("Cookie: __Host-vk_preview={cookie}; theme=dark\r\n");
    let (st, head, body) = proxy_get(port, &authority, "/dash", &ck);
    assert_eq!(st, 200, "{head}\n{}", s.log_tail());
    assert!(body.contains("hello-through-the-proxy"), "{body}");
    let seen = app.seen.lock().unwrap().clone();
    assert!(seen.iter().any(|l| l.starts_with("GET /dash ")), "{seen:?}");
    let status = s.json(&["preview", "status"]);
    let link = status["links"]
        .as_array()
        .unwrap()
        .iter()
        .find(|l| l["machine"] == "fakebox")
        .cloned()
        .unwrap_or_else(|| panic!("no fakebox link: {status}"));
    assert!(link["bytes_in"].as_u64().unwrap() > 100, "{link}");
    assert_eq!(status["proxy"]["routes"][0]["host"], json!(host));
    // `preview url` reports the origin without a credential.
    let u = s.json(&["preview", "url", &target]);
    assert_eq!(u["proxy_url"], json!(format!("http://{authority}/dash")));
    // The token never lands in the event log.
    let ev = s.json(&[
        "api",
        "call",
        "events.read",
        r#"{"types":["preview.opened"]}"#,
    ]);
    assert!(!ev.to_string().contains("vk_token"), "{ev}");

    // Real Chromium (gated; temp profile, never the user's): the one-time link is exchanged for
    // the cookie and the redirected navigation reaches the app over the bridge.
    if std::env::var("VIBEKE_BROWSER_TESTS").is_ok_and(|v| v == "1")
        && let Some(chromium) = playwright_chromium()
    {
        let r = s.json(&["preview", "open", &target, "--proxy", "--no-open"]);
        let url = r["open_url"]
            .as_str()
            .unwrap()
            .replace("/dash?", "/chromium?");
        let profile = tempfile::tempdir().unwrap();
        let mut child = Command::new(&chromium)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .args([
                "--headless=new",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-gpu",
                "--disable-background-networking",
                &format!("--user-data-dir={}", profile.path().display()),
                &url,
            ])
            .spawn()
            .unwrap();
        let t0 = Instant::now();
        while !app
            .seen
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("GET /chromium "))
        {
            if t0.elapsed() > Duration::from_secs(30) {
                let _ = child.kill();
                panic!(
                    "Chromium never got through the proxy: {:?}\n{}",
                    s.json(&["preview", "status"])["proxy"],
                    s.log_tail()
                );
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        let _ = child.kill();
        let _ = child.wait();
    }

    // Mirror: the app's port is in use on this machine (the "remote" is this host) → conflict.
    let e = s.try_json(&["preview", "mirror", &target]).unwrap_err();
    assert!(e.contains("conflict") && e.contains("busy"), "{e}");
    assert_eq!(s.json(&["preview", "status"])["mirrors"], json!([]));

    let _ = s
        .cmd(&["--machine", "fakebox", "server", "stop", "--kill-panes"])
        .output();
}

fn git(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// Task `[previews]` (06 B2 + 05 §6): `task new` declares the repo's previews on ports from
/// the task's lease before anything listens; a server on the leased port turns the preview
/// up; finishing the task retires them.
#[test]
fn task_previews_with_port_leases() {
    let s = Session::new("");
    let repo = s.path().join("repo");
    std::fs::create_dir_all(repo.join(".vibeke")).unwrap();
    std::fs::write(
        repo.join(".vibeke/task.toml"),
        "[ports]\nenv = { PORT = 0, API_PORT = 2 }\n\n[previews]\nweb = { port_env = \"PORT\", path = \"/app\" }\napi = { port_env = \"API_PORT\", label = \"api\" }\nevil = { port = 22 }\n",
    )
    .unwrap();
    std::fs::write(repo.join("README"), "x\n").unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let root = s.path().join("wt");
    let r = s.json(&[
        "task",
        "new",
        "Fix login",
        "--repo",
        &repo.to_string_lossy(),
        "--root",
        &root.to_string_lossy(),
    ]);
    let task = &r["task"];
    let range = task["port_range"].as_array().unwrap();
    let base = range[0].as_u64().unwrap();
    let previews = r["previews"].as_array().unwrap();
    assert_eq!(previews.len(), 2, "{r}");
    let by = |n: &str| previews.iter().find(|p| p["name"] == n).cloned().unwrap();
    assert_eq!(by("web")["port"], json!(base));
    assert_eq!(
        by("web")["url"],
        json!(format!("http://localhost:{base}/app"))
    );
    assert_eq!(by("api")["port"], json!(base + 2));
    assert_eq!(by("api")["label"], "api");
    assert!(
        r["preview_warnings"].to_string().contains("port = 22"),
        "{r}"
    );
    let handle = task["handle"].as_str().unwrap().to_string();
    let listed = s.json(&["preview", "list", "--task", &handle]);
    assert_eq!(listed["previews"].as_array().unwrap().len(), 2, "{listed}");
    // The dev server comes up on the leased port → the preview goes up.
    let _srv = std::net::TcpListener::bind(("127.0.0.1", base as u16)).unwrap();
    s.wait_preview("task preview up", |p| {
        p["port"] == json!(base) && p["status"] == "up"
    });
    // Finishing the task retires its previews.
    s.json(&["task", "finish", &handle]);
    let t0 = Instant::now();
    while s
        .previews()
        .iter()
        .any(|p| p["task_handle"] == json!(handle))
    {
        assert!(t0.elapsed() < Duration::from_secs(10), "{:?}", s.previews());
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn installed_firefox() -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = vec![];
    if let Some(p) = std::env::var_os("VIBEKE_FIREFOX") {
        c.push(PathBuf::from(p));
    }
    c.push("/Applications/Firefox.app/Contents/MacOS/firefox".into());
    if let Some(path) = std::env::var_os("PATH") {
        c.extend(std::env::split_paths(&path).map(|d| d.join("firefox")));
    }
    c.into_iter().find(|p| p.is_file())
}

/// Firefox window profile (06 B3.4), gated: needs `VIBEKE_BROWSER_TESTS=1` **and** an
/// installed Firefox. Headless Firefox on a Vibeke-state profile (`<state>/browser-profiles/
/// fakebox-firefox`, `user.js` with the SOCKS route) loads a remote preview through the
/// peer-checked SOCKS listener and the fake bridge. The user's own Firefox profiles are never
/// touched (explicit `-profile`, `-new-instance`).
#[test]
fn firefox_window_profile_routes_over_socks() {
    if std::env::var("VIBEKE_BROWSER_TESTS").ok().as_deref() != Some("1") {
        eprintln!("VIBEKE_BROWSER_TESTS != 1; skipping");
        return;
    }
    let Some(ff) = installed_firefox() else {
        eprintln!("Firefox not installed; skipping");
        return;
    };
    let mut s = Session::new(&format!(
        "[[remote.machine]]\nlabel = \"fakebox\"\naddress = \"fakebox\"\n\n[preview]\nprofile_browser = \"firefox\"\nbrowser = \"{}\"\n",
        ff.display()
    ));
    let ssh = fake_ssh(s.path());
    s.extra_env = vec![
        ("VIBEKE_SSH".into(), ssh.to_string_lossy().into_owned()),
        ("VIBEKE_TEST_HOOKS".into(), "1".into()),
    ];
    let app = http_server("hello-firefox");
    let d = s.json(&[
        "--machine",
        "fakebox",
        "preview",
        "declare",
        &app.port.to_string(),
    ]);
    let rh = d["preview"]["handle"].as_str().unwrap().to_string();
    let r = s.json(&[
        "preview",
        "open",
        &format!("fakebox/{rh}"),
        "--window",
        "--headless",
    ]);
    assert_eq!(r["browser_kind"], "firefox", "{r}");
    assert_eq!(r["profile"], "fakebox-firefox");
    let dir = PathBuf::from(r["profile_dir"].as_str().unwrap());
    assert!(dir.starts_with(s.path()), "{}", dir.display());
    let prefs = std::fs::read_to_string(dir.join("user.js")).unwrap();
    let socks = r["socks_port"].as_u64().unwrap();
    assert!(
        prefs.contains(&format!("network.proxy.socks_port\", {socks}")),
        "{prefs}"
    );
    let pid = r["pid"].as_u64().unwrap() as i32;
    let t0 = Instant::now();
    while !app
        .seen
        .lock()
        .unwrap()
        .iter()
        .any(|l| l.starts_with("GET / "))
    {
        if t0.elapsed() > Duration::from_secs(40) {
            // SAFETY: plain kill of the browser the server launched for this test.
            unsafe { libc::kill(pid, libc::SIGTERM) };
            panic!(
                "Firefox never fetched through the route: {}\n{}",
                s.json(&["preview", "status"]),
                s.log_tail()
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    // SAFETY: plain kill of the browser the server launched for this test.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    let _ = s
        .cmd(&["--machine", "fakebox", "server", "stop", "--kill-panes"])
        .output();
}
