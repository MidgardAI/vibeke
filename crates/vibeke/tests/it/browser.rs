//! Goal 03 Stage 3 end to end with the real binary: `vibeke mcp` against a real server,
//! `vibeke integration install <h> --mcp` into temp harness dirs, and — with
//! `VIBEKE_BROWSER_TESTS=1` — the agents' headless browser on Playwright's Chromium (never the
//! user's browser or profiles; nothing is downloaded): a page that tries a forbidden loopback
//! port, the metadata address, an undeclared WebSocket and a redirect to a forbidden port gets
//! all of them denied and logged, while the declared preview loads; screenshots are saved.

use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

struct Session {
    dir: tempfile::TempDir,
}

impl Session {
    fn new(config: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("vkbrw")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Session { dir }
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
            // Harness configs always point into the temp dir.
            .env("CLAUDE_CONFIG_DIR", d.join("claude"))
            .env("CODEX_HOME", d.join("codex"));
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "VIBEKE_PANE_ID",
        ] {
            c.env_remove(k);
        }
        c.args(args);
        c
    }
    fn try_json(&self, args: &[&str]) -> Result<Value, Value> {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.cmd(&full).output().unwrap();
        if !out.status.success() {
            let e: Value = serde_json::from_slice(&out.stderr)
                .unwrap_or_else(|_| json!({"raw": String::from_utf8_lossy(&out.stderr)}));
            return Err(e);
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
        let lines: Vec<&str> = log.lines().collect();
        lines[lines.len().saturating_sub(40)..].join("\n")
    }
    fn events(&self, kind: &str) -> Vec<Value> {
        self.json(&["events", "read", "--types", kind, "--limit", "1000"])["events"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

/// `vibeke mcp` as a child, talking JSON-RPC lines.
struct Mcp {
    child: std::process::Child,
    out: BufReader<std::process::ChildStdout>,
    next: u64,
}

impl Mcp {
    fn start(s: &Session) -> Mcp {
        let mut child = s
            .cmd(&["mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let out = BufReader::new(child.stdout.take().unwrap());
        Mcp {
            child,
            out,
            next: 1,
        }
    }
    fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next;
        self.next += 1;
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{line}").unwrap();
        stdin.flush().unwrap();
        let mut resp = String::new();
        self.out.read_line(&mut resp).unwrap();
        let v: Value = serde_json::from_str(&resp).unwrap_or_else(|e| panic!("{e}: {resp:?}"));
        assert_eq!(v["id"], id);
        v
    }
    fn notify(&mut self, method: &str) {
        let stdin = self.child.stdin.as_mut().unwrap();
        writeln!(stdin, "{}", json!({"jsonrpc": "2.0", "method": method})).unwrap();
    }
    fn tool(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({"name": name, "arguments": args}))["result"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn mcp_against_a_real_server_and_mcp_installers() {
    let s = Session::new("");
    // Start the server.
    s.json(&["server", "status"]);
    let mut m = Mcp::start(&s);
    let init = m.request(
        "initialize",
        json!({"protocolVersion": "2025-11-25", "capabilities": {}, "clientInfo": {"name": "test", "version": "0"}}),
    );
    assert_eq!(init["result"]["protocolVersion"], "2025-11-25");
    m.notify("notifications/initialized");
    let tools = m.request("tools/list", json!({}));
    assert!(tools["result"]["tools"].as_array().unwrap().len() >= 13);
    let r = m.tool("preview_declare", json!({"port": 1, "label": "nothing"}));
    assert_eq!(r["isError"], false, "{r}");
    let r = m.tool("preview_list", json!({}));
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("\"nothing\""),
        "{r}"
    );
    // Refused before any browser starts (works without Chromium installed).
    for (url, reason) in [
        ("http://127.0.0.1:5432/", "loopback_port_not_a_preview"),
        ("http://169.254.169.254/", "metadata_address"),
        ("file:///etc/passwd", "scheme_not_allowed"),
    ] {
        let r = m.tool("browser_open", json!({"url": url}));
        assert_eq!(r["isError"], true, "{url}: {r}");
        let t = r["content"][0]["text"].as_str().unwrap();
        assert!(
            t.starts_with("destination_denied:") && t.contains(reason),
            "{t}"
        );
    }
    let r = m.tool("browser_click", json!({"session": "b99", "x": 1, "y": 1}));
    assert_eq!(r["isError"], true);
    assert!(
        r["content"][0]["text"]
            .as_str()
            .unwrap()
            .starts_with("not_found:")
    );
    drop(m);
    // api.methods lists browser.*.
    let methods = s.json(&["api", "methods"]);
    assert!(
        methods["methods"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["name"] == "browser.screenshot")
    );
    // `browser install` without --yes and without a terminal asks and stops (no download).
    let out = s
        .cmd(&["browser", "install"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    // The headless shell, or the full browser where the machine can show windows.
    assert!(err.contains("chrome-for-testing-public"), "{err}");
    // Nothing was started or downloaded.
    assert!(!s.path().join("state/default/agent-browser").exists());

    // `integration install <h> --mcp` into the redirected (temp) dirs.
    std::fs::create_dir_all(s.path().join("claude")).unwrap();
    std::fs::create_dir_all(s.path().join("codex")).unwrap();
    std::fs::write(
        s.path().join("codex/config.toml"),
        "# mine\nmodel = \"x\"\n",
    )
    .unwrap();
    for h in ["claude", "codex"] {
        let out = s
            .cmd(&["integration", "install", h, "--mcp"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let cj: Value = serde_json::from_str(
        &std::fs::read_to_string(s.path().join("claude/.claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(cj["mcpServers"]["vibeke"]["args"], json!(["mcp"]));
    let toml = std::fs::read_to_string(s.path().join("codex/config.toml")).unwrap();
    assert!(toml.starts_with("# mine\n"), "{toml}");
    assert!(toml.contains("[mcp_servers.vibeke]"), "{toml}");
    let st = s
        .cmd(&["integration", "status", "all", "--mcp"])
        .output()
        .unwrap();
    let st = String::from_utf8_lossy(&st.stdout);
    assert_eq!(st.matches("mcp installed").count(), 2, "{st}");
    for h in ["claude", "codex"] {
        let out = s
            .cmd(&["integration", "uninstall", h, "--mcp"])
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    let toml = std::fs::read_to_string(s.path().join("codex/config.toml")).unwrap();
    assert!(!toml.contains("vibeke"), "{toml}");
}

// ---- real Chromium (VIBEKE_BROWSER_TESTS=1) ---------------------------------------------------

fn browser_tests() -> bool {
    std::env::var("VIBEKE_BROWSER_TESTS").is_ok_and(|v| v == "1")
}

/// Playwright's headless shell on disk (never downloaded here).
fn playwright_shell() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let mut best: Option<(u32, String)> = None;
    for root in [
        format!("{home}/Library/Caches/ms-playwright"),
        format!("{home}/.cache/ms-playwright"),
    ] {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(rev) = name
                .strip_prefix("chromium_headless_shell-")
                .and_then(|r| r.parse::<u32>().ok())
            else {
                continue;
            };
            for sub in [
                "chrome-headless-shell-mac-arm64/chrome-headless-shell",
                "chrome-headless-shell-mac-x64/chrome-headless-shell",
                "chrome-headless-shell-linux64/chrome-headless-shell",
            ] {
                let p = e.path().join(sub);
                if p.is_file() && best.as_ref().is_none_or(|b| b.0 < rev) {
                    best = Some((rev, p.display().to_string()));
                }
            }
        }
    }
    best.map(|b| b.1)
}

/// A loopback HTTP server: `pages` by path (`/redir` → 302 to `redirect`); records requests.
struct Http {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
}

fn http_server(pages: Vec<(&'static str, String)>, redirect: Option<String>) -> Http {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen2 = seen.clone();
    let pages = Arc::new(pages);
    std::thread::spawn(move || {
        for st in l.incoming() {
            let Ok(mut st) = st else { continue };
            let (seen, pages, redirect) = (seen2.clone(), pages.clone(), redirect.clone());
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 8192];
                let mut got = 0;
                let _ = st.set_read_timeout(Some(Duration::from_secs(5)));
                while got < buf.len() {
                    match st.read(&mut buf[got..]) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => got += n,
                    }
                    if buf[..got].windows(4).any(|w| w == b"\r\n\r\n") {
                        break;
                    }
                }
                let req = String::from_utf8_lossy(&buf[..got]).into_owned();
                let first = req.lines().next().unwrap_or("").to_string();
                seen.lock().unwrap().push(first.clone());
                let path = first.split(' ').nth(1).unwrap_or("/").to_string();
                if path == "/redir"
                    && let Some(r) = &redirect
                {
                    let _ = write!(
                        st,
                        "HTTP/1.1 302 Found\r\nLocation: {r}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    );
                    return;
                }
                let body = pages
                    .iter()
                    .find(|(p, _)| *p == path)
                    .map(|(_, b)| b.clone())
                    .unwrap_or_else(|| "<html><body>none</body></html>".into());
                let _ = write!(
                    st,
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            });
        }
    });
    Http { port, seen }
}

fn wait_until(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[test]
fn headless_browser_filters_destinations_with_real_chromium() {
    if !browser_tests() {
        eprintln!("skipped: set VIBEKE_BROWSER_TESTS=1");
        return;
    }
    let Some(shell) = playwright_shell() else {
        eprintln!("skipped: no Playwright chrome-headless-shell on disk");
        return;
    };
    // A loopback service the page must not reach (stands in for a database/admin UI).
    let secret = http_server(vec![("/", "<p>secret</p>".into())], None);
    let page = format!(
        r#"<html><head><title>Preview</title></head><body>
<h1>Hello preview</h1><button id=go onclick="document.getElementById('out').textContent='clicked'">Save</button>
<p id=out></p><input id=q>
<img src="http://169.254.169.254/latest/meta-data/">
<script>
  console.log('page loaded');
  fetch('http://127.0.0.1:{secret}/steal').catch(e => console.error('fetch blocked: ' + e));
  try {{ const ws = new WebSocket('ws://127.0.0.1:{secret}/hmr'); ws.onerror = () => console.error('ws blocked'); }} catch (e) {{}}
  setTimeout(() => {{ throw new Error('boom from page'); }}, 10);
</script></body></html>"#,
        secret = secret.port
    );
    let app = http_server(
        vec![("/", page)],
        Some(format!("http://127.0.0.1:{}/redirected", secret.port)),
    );
    let s = Session::new(&format!(
        "[preview]\nbrowser_path = \"{shell}\"\nbrowser_idle = \"30s\"\nbrowser_external = \"deny\"\n"
    ));
    s.json(&[
        "preview",
        "declare",
        &app.port.to_string(),
        "--label",
        "app",
    ]);
    let open = s.json(&["browser", "open", "v1"]);
    let sid = open["session"].as_str().unwrap().to_string();
    assert_eq!(open["status"], 200, "{open}");
    assert_eq!(open["title"], "Preview");
    assert_eq!(open["environment"]["kind"], "remote_headless");
    assert!(
        open["environment"]["browser"]
            .as_str()
            .unwrap()
            .contains("HeadlessChrome")
    );
    // The page's forbidden requests are denied and logged.
    wait_until("denials logged", Duration::from_secs(15), || {
        let n = s.json(&["browser", "network", &sid, "--failed"]);
        let blocked: Vec<String> = n["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| !e["blocked_by_policy"].is_null())
            .map(|e| e["url"].as_str().unwrap_or("").to_string())
            .collect();
        blocked.iter().any(|u| u.contains("169.254.169.254"))
            && blocked
                .iter()
                .filter(|u| u.contains(&format!(":{}", secret.port)))
                .count()
                >= 2
    });
    let net = s.json(&["browser", "network", &sid, "--failed"]);
    let reasons: Vec<&str> = net["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["blocked_by_policy"].as_str())
        .collect();
    assert!(reasons.contains(&"metadata_address"), "{net:#}");
    assert!(reasons.contains(&"loopback_port_not_a_preview"), "{net:#}");
    // Both layers took part: the Fetch layer (fetch/img by IP literal) and the proxy (the
    // WebSocket, which Fetch interception never sees).
    let layers: Vec<&str> = net["entries"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["layer"].as_str())
        .collect();
    assert!(
        layers.contains(&"fetch") && layers.contains(&"proxy"),
        "{net:#}"
    );
    assert!(
        secret.seen.lock().unwrap().is_empty(),
        "the forbidden loopback service was reached: {:?}",
        secret.seen.lock().unwrap()
    );
    let denied = s.events("browser.request_denied");
    assert!(denied.len() >= 3, "{denied:#?}");
    // Console capture: the thrown exception and our log line.
    let cons = s.json(&["browser", "console", &sid, "--level", "error"]);
    assert!(
        cons["entries"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["text"].as_str().unwrap_or("").contains("boom from page")),
        "{cons:#}"
    );
    // Drive the page.
    s.json(&["browser", "click", &sid, "text=Save"]);
    s.json(&["browser", "type", &sid, "#q", "hello"]);
    let snap = s.json(&["browser", "snapshot", &sid]);
    let text = snap["content"].as_str().unwrap();
    assert!(text.contains("button \"Save\""), "{text}");
    assert!(text.contains("clicked"), "{text}");
    // Navigations to forbidden destinations (directly and via a redirect) are errors.
    let e = s
        .try_json(&[
            "browser",
            "navigate",
            &sid,
            &format!("http://127.0.0.1:{}/", secret.port),
        ])
        .unwrap_err();
    assert_eq!(e["error"]["kind"], "destination_denied", "{e}");
    let e = s
        .try_json(&[
            "browser",
            "navigate",
            &sid,
            &format!("http://127.0.0.1:{}/redir", app.port),
        ])
        .unwrap_err();
    assert_eq!(e["error"]["kind"], "destination_denied", "{e}");
    assert_eq!(
        e["error"]["details"]["reason"],
        "loopback_port_not_a_preview"
    );
    let e = s
        .try_json(&["browser", "navigate", &sid, "file:///etc/hosts"])
        .unwrap_err();
    assert_eq!(e["error"]["kind"], "destination_denied");
    assert!(secret.seen.lock().unwrap().is_empty());
    // Back to the preview; screenshot saved as a blob and written locally.
    s.json(&["browser", "navigate", &sid, "/"]);
    let out = s.path().join("shot.png");
    let shot = s.json(&[
        "browser",
        "screenshot",
        &sid,
        "--out",
        out.to_str().unwrap(),
    ]);
    let png = std::fs::read(&out).unwrap();
    assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
    assert!(shot["width"].as_u64().unwrap() >= 100);
    assert!(Path::new(shot["path_on_machine"].as_str().unwrap()).is_file());
    assert_eq!(shot["meta"]["environment"]["kind"], "remote_headless");
    assert!(shot.get("data_b64").is_none());
    // MCP screenshot returns image content.
    let mut m = Mcp::start(&s);
    m.request("initialize", json!({"protocolVersion": "2025-06-18"}));
    let r = m.tool("browser_screenshot", json!({"session": sid}));
    assert_eq!(r["content"][0]["type"], "image", "{r}");
    assert!(r["content"][0]["data"].as_str().unwrap().len() > 100);
    // Take-over: the user holds it, so agent calls would fail; release.
    s.json(&["browser", "take-over", &sid]);
    assert_eq!(
        s.json(&["browser", "list"])["sessions"][0]["human_control"],
        true
    );
    s.json(&["browser", "release", &sid]);
    s.json(&["browser", "close", &sid]);
    assert_eq!(
        s.json(&["browser", "list"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // Only the app (and never the secret) saw traffic.
    assert!(!app.seen.lock().unwrap().is_empty());
    assert!(secret.seen.lock().unwrap().is_empty());
}

// ---- Goal 03 Stage 4: screenshots as evidence ----------------------------------------------

fn git(repo: &Path, args: &[&str]) -> String {
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
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn temp_repo(s: &Session) -> std::path::PathBuf {
    let repo = s.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("index.html"), "<h1>blue</h1>\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    repo
}

#[test]
fn screenshot_cli_code_state_and_api_without_a_browser() {
    let s = Session::new("");
    let repo = temp_repo(&s);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    // `vibeke screenshot code-state` runs locally (no server needed).
    let out = s
        .cmd(&["screenshot", "code-state", repo.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    let c: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(c["head_sha"], head);
    assert_eq!(c["dirty_state"], "clean");
    assert_eq!(c["dirty_digest"], Value::Null);
    std::fs::write(repo.join("index.html"), "<h1>red</h1>\n").unwrap();
    let out = s
        .cmd(&["screenshot", "code-state", repo.to_str().unwrap()])
        .output()
        .unwrap();
    let c: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(c["dirty_state"], "dirty");
    assert!(c["dirty_digest"].as_str().unwrap().len() == 64);
    let out = s
        .cmd(&[
            "screenshot",
            "code-state",
            s.path().join("nope").to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!out.status.success());
    // API against a real server.
    let l = s.json(&["screenshot", "list"]);
    assert_eq!(l["count"], 0);
    let e = s.try_json(&["screenshot", "get", "s9"]).unwrap_err();
    assert_eq!(e["error"]["kind"], "not_found");
    let e = s.try_json(&["browser", "diff", "s1", "s2"]).unwrap_err();
    assert_eq!(e["error"]["kind"], "not_found");
    let methods = s.json(&["api", "methods"]);
    for m in [
        "screenshot.list",
        "screenshot.get",
        "screenshot.delete",
        "browser.diff",
    ] {
        assert!(
            methods["methods"]
                .as_array()
                .unwrap()
                .iter()
                .any(|x| x["name"] == m),
            "{m}"
        );
    }
}

#[test]
fn screenshots_carry_environment_code_state_and_binding_with_real_chromium() {
    if !browser_tests() {
        eprintln!("skipped: set VIBEKE_BROWSER_TESTS=1");
        return;
    }
    let Some(shell) = playwright_shell() else {
        eprintln!("skipped: no Playwright chrome-headless-shell on disk");
        return;
    };
    let s = Session::new(&format!(
        "[preview]\nbrowser_path = \"{shell}\"\nbrowser_idle = \"30s\"\nbrowser_external = \"deny\"\n"
    ));
    let repo = temp_repo(&s);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    // The app was "built" from this checkout: its dev script serves the code state.
    let out = s
        .cmd(&["screenshot", "code-state", repo.to_str().unwrap()])
        .output()
        .unwrap();
    let build = String::from_utf8(out.stdout).unwrap().trim().to_string();
    let app = http_server(
        vec![
            (
                "/",
                "<html><head><title>Blue</title></head><body style='background:#00f'><h1>Blue</h1></body></html>".into(),
            ),
            (
                "/red",
                "<html><head><title>Red</title></head><body style='background:#f00'><h1>Red</h1></body></html>".into(),
            ),
            ("/__vibeke_build", build),
        ],
        None,
    );
    let pane = s.json(&["workspace", "create", "--cwd", repo.to_str().unwrap()])["root_pane"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    s.json(&[
        "preview",
        "declare",
        &app.port.to_string(),
        "--pane",
        &pane,
        "--label",
        "app",
    ]);
    let open = s.json(&["browser", "open", "v1", "--viewport", "640x400"]);
    let sid = open["session"].as_str().unwrap().to_string();
    assert_eq!(open["status"], 200, "{open}");

    let shot = s.json(&["browser", "screenshot", &sid]);
    let m = &shot["meta"];
    assert_eq!(shot["handle"], "s1");
    assert_eq!(m["environment"]["kind"], "remote_headless");
    assert_eq!(m["environment"]["fresh_context"], true);
    assert_eq!(m["environment"]["viewport"]["width"], 640);
    assert!(
        m["environment"]["browser"]
            .as_str()
            .unwrap()
            .contains("HeadlessChrome")
    );
    assert!(m["environment"]["browser_version"].is_string());
    assert!(
        m["label"]
            .as_str()
            .unwrap()
            .contains("headless · fresh context")
    );
    assert_eq!(m["preview"], "v1");
    assert_eq!(m["title"], "Blue");
    assert!(
        m["final_url"]
            .as_str()
            .unwrap()
            .starts_with(&format!("http://localhost:{}/", app.port))
    );
    assert_eq!(m["code"]["head_sha"], head, "{m:#}");
    assert_eq!(m["code"]["dirty_state"], "clean");
    assert_eq!(m["runtime"]["status"], "known", "{m:#}");
    assert_eq!(m["runtime"]["source"], "probe");
    assert_eq!(m["binding"], "bound", "{m:#}");
    assert!(
        app.seen
            .lock()
            .unwrap()
            .iter()
            .any(|l| l.starts_with("GET /__vibeke_build"))
    );

    // The checkout moves on while the server still serves the old build → illustrative.
    std::fs::write(repo.join("index.html"), "<h1>red</h1>\n").unwrap();
    git(&repo, &["commit", "-qam", "red"]);
    s.json(&["browser", "navigate", &sid, "/red"]);
    let shot2 = s.json(&["browser", "screenshot", &sid]);
    assert_eq!(shot2["meta"]["binding"], "illustrative");
    assert!(
        shot2["meta"]["binding_reason"]
            .as_str()
            .unwrap()
            .contains("Running build is"),
        "{:#}",
        shot2["meta"]
    );
    let ev = s.events("screenshot.captured");
    assert_eq!(ev.len(), 2, "{ev:#?}");
    assert_eq!(ev[0]["data"]["binding"], "bound");

    // List/get/open.
    let l = s.json(&["screenshot", "list", "--preview", "v1"]);
    assert_eq!(l["count"], 2);
    assert_eq!(l["screenshots"][0]["handle"], "s2");
    let g = s.json(&["screenshot", "get", "s1"]);
    assert_eq!(g["binding"], "bound");
    assert!(Path::new(g["path_on_machine"].as_str().unwrap()).is_file());
    let out = s
        .cmd(&["--json", "screenshot", "open", "s1"])
        .env("VIBEKE_NO_OPEN", "1")
        .output()
        .unwrap();
    assert!(out.status.success());
    let o: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(o["opened"], false);
    assert_eq!(
        &std::fs::read(o["out"].as_str().unwrap()).unwrap()[..8],
        b"\x89PNG\r\n\x1a\n"
    );

    // Visual diff blue → red: everything changed except maybe nothing; diff image written.
    let diff_out = s.path().join("diff.png");
    let d = s.json(&[
        "browser",
        "diff",
        "s1",
        "s2",
        "--out",
        diff_out.to_str().unwrap(),
    ]);
    assert!(d["changed_ratio"].as_f64().unwrap() > 0.5, "{d:#}");
    assert_eq!(d["width"], 640);
    assert!(!d["regions"].as_array().unwrap().is_empty());
    assert_eq!(
        &std::fs::read(&diff_out).unwrap()[..8],
        b"\x89PNG\r\n\x1a\n"
    );
    // MCP browser_diff returns the diff image.
    let mut mcp = Mcp::start(&s);
    mcp.request("initialize", json!({"protocolVersion": "2025-06-18"}));
    let r = mcp.tool("browser_diff", json!({"a": "s1", "b": "s1"}));
    assert_eq!(r["isError"], false, "{r}");
    assert_eq!(r["content"][0]["type"], "image", "{r}");
    assert!(
        r["content"][1]["text"]
            .as_str()
            .unwrap()
            .contains("\"changed_pixels\": 0")
    );
    s.json(&["browser", "close", &sid]);
}

/// Device presets, one-shot screenshots and `preview.console_error` against Playwright's
/// headless shell (gated; never the user's browser or profiles).
#[test]
fn device_one_shot_and_console_errors_with_real_chromium() {
    if !browser_tests() {
        eprintln!("skipped: set VIBEKE_BROWSER_TESTS=1");
        return;
    }
    let Some(shell) = playwright_shell() else {
        eprintln!("skipped: no Playwright chrome-headless-shell on disk");
        return;
    };
    let s = Session::new(&format!(
        "[preview]\nbrowser_path = \"{shell}\"\nbrowser_idle = \"30s\"\nbrowser_external = \"deny\"\n"
    ));
    let app = http_server(
        vec![(
            "/",
            "<html><head><title>Dev</title></head><body><h1>Dev</h1><script>console.error('boom token=abcd1234abcd1234'); throw new Error('kaput');</script></body></html>".into(),
        )],
        None,
    );
    s.json(&[
        "preview",
        "declare",
        &app.port.to_string(),
        "--label",
        "app",
    ]);
    // One-shot, no session: the device's viewport and DPR end up in the screenshot record.
    let shot = s.json(&["browser", "screenshot", "v1", "--device", "iphone-15"]);
    assert_eq!(shot["one_shot"], true, "{shot}");
    let env = &shot["meta"]["environment"];
    assert_eq!(env["device"], "iphone-15");
    assert_eq!(env["viewport"]["width"], 393);
    assert_eq!(env["dpr"], 3.0);
    // 393 x 852 CSS px at DPR 3.
    assert_eq!(shot["width"], 1179, "{shot}");
    assert_eq!(
        s.json(&["browser", "list"])["sessions"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    // The page's uncaught exception and console.error reach the event log, redacted.
    wait_until("preview.console_error", Duration::from_secs(20), || {
        !s.events("preview.console_error").is_empty()
    });
    let ev = s.events("preview.console_error");
    assert_eq!(ev[0]["subject"]["preview"], "v1");
    let all = serde_json::to_string(&ev).unwrap();
    assert!(!all.contains("abcd1234abcd1234"), "{all}");
    // A session with a device keeps it in its summary.
    let open = s.json(&["browser", "open", "v1", "--device", "pixel-8"]);
    assert_eq!(open["device"], "pixel-8");
    s.json(&["browser", "close", open["session"].as_str().unwrap()]);
}
