//! Goal 03 Codex review follow-up tests for the browser pane (kept in their own file so the
//! pane module's own tests stay untouched).

use super::*;
use crate::ServerOpts;
use crate::paths::Paths;
use std::sync::Once;
use vk_proto::model::Pane;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-pane-followup-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
            if std::env::var_os("VIBEKE_RUNTIME_DIR").is_none() {
                std::env::set_var("VIBEKE_RUNTIME_DIR", base.join("run"));
            }
            if std::env::var_os("VIBEKE_STATE_DIR").is_none() {
                std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            }
            if std::env::var_os("VIBEKE_CONFIG").is_none() {
                std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
            }
        }
    });
}

pub(super) struct Env {
    pub _dir: tempfile::TempDir,
    pub server: Arc<Server>,
}

pub(super) fn ctx(scope: Option<&str>) -> Ctx {
    Ctx {
        client_id: format!("c-{}", scope.unwrap_or("user")),
        kind: "cli".into(),
        pane_scope: scope.map(str::to_string),
        remote: false,
    }
}

impl Env {
    pub fn new() -> Env {
        init_env();
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = Paths {
            session: "t".into(),
            runtime: root.join("run"),
            state: root.join("state"),
        };
        let opts = ServerOpts {
            session: "t".into(),
            machine: "testbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
        };
        let server = Server::new(paths, opts).unwrap();
        let e = Env { _dir: dir, server };
        e.put_pane("pane-a", "ws-a", None);
        e
    }

    pub fn put_pane(&self, id: &str, ws: &str, browser: Option<BrowserPane>) {
        let p = Pane {
            id: id.into(),
            handle: id.into(),
            tab: format!("tab-{ws}"),
            workspace: ws.into(),
            title: None,
            auto_title: String::new(),
            cwd: None,
            cols: 80,
            rows: 24,
            child_pid: None,
            fg_cmdline: vec![],
            exited: false,
            exit_code: None,
            unread: false,
            marked_unread: false,
            pinned: false,
            created_by: "user".into(),
            recovered: None,
            isolation: Default::default(),
            browser,
        };
        let mut c = self.server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.pane(p);
        self.server.commit(&mut c, tx).unwrap();
    }

    /// One JSON-RPC call through the real dispatch (authorization included).
    pub async fn call(&self, ctx: &Ctx, method: &str, params: Value) -> Value {
        let line = json!({"jsonrpc": "2.0", "id": 1, "method": method, "params": params});
        let out = crate::api::handle_line(&self.server, ctx, &line.to_string()).await;
        serde_json::from_str(&out).unwrap()
    }
}

fn denied(v: &Value) -> Option<String> {
    let e = v.get("error")?;
    (e["data"]["kind"] == "permission_denied" || e.to_string().contains("permission_denied"))
        .then(|| e["message"].as_str().unwrap_or_default().to_string())
}

/// Finding 1: a remote-profile pane browser (SOCKS route) gets the WebRTC policy in the form
/// both the headless shell and full Chromium read; a local one has no proxy and no policy.
#[test]
fn pane_launch_flags_keep_webrtc_on_the_route() {
    let req = LaunchReq {
        profile: "devbox".into(),
        profile_dir: "/state/browser-profiles/devbox".into(),
        dpr: 2.0,
        socks_port: Some(41000),
        log: None,
    };
    for bin in ["/pw/chrome-headless-shell", "/pw/Google Chrome for Testing"] {
        let a = pane_launch_options(std::path::Path::new(bin), &req).args();
        assert!(a.contains(&"--proxy-server=socks5://127.0.0.1:41000".to_string()));
        assert!(
            a.contains(&"--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()),
            "{a:?}"
        );
        assert!(a.contains(&"--webrtc-ip-handling-policy=disable_non_proxied_udp".to_string()));
        assert!(!a.iter().any(|x| x == "--force-webrtc-ip-handling-policy"));
    }
    let local = LaunchReq {
        socks_port: None,
        ..req
    };
    let a = pane_launch_options(std::path::Path::new("/pw/chrome-headless-shell"), &local).args();
    assert!(!a.iter().any(|x| x.starts_with("--proxy-server")));
}

/// Finding 4: URLs are checked and stored in the canonical form Chromium loads.
#[test]
fn urls_are_canonical_and_parsed_like_the_browser() {
    assert_eq!(
        normalize_url("http://192.168.1.10\\@localhost:5173/").as_deref(),
        Some("http://192.168.1.10/@localhost:5173/")
    );
    assert_eq!(normalize_url("http://localhost@10.0.0.1/"), None);
    assert_eq!(
        normalize_url("2130706433:5173").as_deref(),
        Some("http://127.0.0.1:5173/")
    );
    // A remote owner's record may only start on that machine's loopback.
    assert_eq!(
        initial_url("http://192.168.1.10\\@localhost:5173/", true),
        None
    );
    assert_eq!(
        initial_url("http://[::ffff:192.168.1.10]:5173/", true),
        None
    );
    assert_eq!(
        initial_url("http://0x7f.1:5173/", true).as_deref(),
        Some("http://127.0.0.1:5173/")
    );
    assert_eq!(
        initial_url("http://[::ffff:127.0.0.1]:5173/", true).as_deref(),
        Some("http://[::ffff:7f00:1]:5173/")
    );
    assert_eq!(
        initial_url("http://0.0.0.0:5173/", true).as_deref(),
        Some("http://0.0.0.0:5173/")
    );
    assert_eq!(initial_url("http://u:p@localhost:5173/", true), None);
    assert_eq!(
        initial_url("https://example.com/", false).as_deref(),
        Some("https://example.com/")
    );
}

/// Findings 4 and 6 through the real dispatch: a pane token can't open a non-loopback URL
/// disguised with a backslash, and can't create a browser pane (or open a preview) for
/// another machine; the same calls with a full-scope token get past authorization.
#[tokio::test(flavor = "multi_thread")]
async fn pane_scope_cannot_create_cross_machine_or_disguised_panes() {
    let e = Env::new();
    let pane = ctx(Some("pane-a"));
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"url": "http://localhost:5173/", "machine": "devbox", "pane": "pane-a"}),
        )
        .await;
    let why = denied(&r).unwrap_or_else(|| panic!("cross-machine create allowed: {r}"));
    assert!(why.contains("another machine"), "{why}");
    let r = e
        .call(
            &pane,
            "preview.open",
            json!({"url": "http://localhost:5173/", "machine": "devbox"}),
        )
        .await;
    assert!(denied(&r).is_some(), "{r}");
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"preview": "devbox/p1", "pane": "pane-a"}),
        )
        .await;
    assert!(denied(&r).is_some(), "{r}");
    // The Codex counterexample: WHATWG says the host is 192.168.1.10.
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"url": "http://192.168.1.10\\@localhost:5173/", "pane": "pane-a"}),
        )
        .await;
    let why = denied(&r).unwrap_or_else(|| panic!("disguised LAN URL allowed: {r}"));
    assert!(why.contains("loopback"), "{why}");
    let r = e
        .call(
            &pane,
            "preview.open",
            json!({"url": "http://192.168.1.10\\@localhost:5173/", "window": true}),
        )
        .await;
    assert!(denied(&r).is_some(), "{r}");
    // Naming this machine is fine from a pane (authorization passes; the pane is created).
    let r = e
        .call(
            &pane,
            "browser.pane.create",
            json!({"url": "http://localhost:5173/", "machine": "testbox", "pane": "pane-a"}),
        )
        .await;
    assert!(denied(&r).is_none(), "{r}");
    // Full scope may pick another machine (the pane record is created; nothing launches
    // until a client views it).
    let r = e
        .call(
            &ctx(None),
            "browser.pane.create",
            json!({"url": "http://localhost:5173/", "machine": "devbox", "pane": "pane-a"}),
        )
        .await;
    assert!(denied(&r).is_none(), "{r}");
}
