//! Catalog-versus-dispatch authorization (09 §5.2): the pane scope the API catalog reports
//! (`api::pane_scope_of`, the source of `docs/api/methods.json`) must match what dispatch
//! actually does. Every `Forbidden` method is refused at `authorize`; no `Open` method may be
//! refused to a pane token for params a full-scope caller is not refused for, except the
//! listed conditional denials. Runs in-process against a throwaway server (no browser, holders
//! spawn `/bin/false`), every call bounded by a timeout.

use crate::api::{PaneScope, dispatch, pane_scope_of};
use crate::core::Tx;
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use serde_json::{Value, json};
use std::sync::{Arc, Once};
use std::time::Duration;
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, RpcError};

/// Every method table in the server (the same set `crates/vibeke/tests/api_docs.rs` catalogs).
fn tables() -> Vec<&'static [(&'static str, bool)]> {
    vec![
        crate::api::METHODS,
        crate::agents::METHODS,
        crate::review::t4::METHODS,
        crate::preview::METHODS,
        crate::sandbox::METHODS,
        crate::agent_browser::METHODS,
        crate::browser_pane::METHODS,
        crate::parity::METHODS,
        crate::screenshots::METHODS,
        crate::desk::METHODS,
        crate::drafts::METHODS,
        crate::assist::METHODS,
        crate::compat::METHODS,
        crate::notify::METHODS,
        crate::theme::METHODS,
        crate::layouts::METHODS,
        crate::session_api::METHODS,
        crate::config_api::METHODS,
        crate::blob_api::METHODS,
        crate::pane_api::METHODS,
        crate::task_park::METHODS,
        crate::security::METHODS,
        crate::collision::METHODS,
    ]
}

fn methods() -> Vec<&'static str> {
    let mut v: Vec<&str> = tables()
        .into_iter()
        .flat_map(|t| t.iter().map(|(n, _)| *n))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// `Open` methods whose handler refuses a pane token for `{}` params but not for every param
/// set (so they are not full-scope-only). Each needs a reason.
const CONDITIONAL_DENIALS: &[(&str, &str)] = &[(
    "browser.eval",
    "allowed from a pane once `preview.browser_script = true` grants the browser.script capability",
)];

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // The sweep calls hundreds of methods with one pane token: test authorization, not
        // the request budget (limits.rs has its own tests).
        crate::limits::UNLIMITED.store(true, std::sync::atomic::Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!("vk-scope-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them.
        unsafe {
            std::env::set_var("VIBEKE_NO_OPEN", "1");
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

struct Env {
    _dir: tempfile::TempDir,
    server: Arc<Server>,
}

fn ctx_full() -> crate::api::Ctx {
    crate::api::Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

fn ctx_pane() -> crate::api::Ctx {
    crate::api::Ctx {
        client_id: "c-pane-a".into(),
        kind: "cli".into(),
        pane_scope: Some("pane-a".into()),
        remote: false,
    }
}

impl Env {
    fn new() -> Env {
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
        let p = Pane {
            id: "pane-a".into(),
            handle: "pane-a".into(),
            tab: "tab-a".into(),
            workspace: "ws-a".into(),
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
            browser: None,
        };
        {
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.pane(p);
            server.commit(&mut c, tx).unwrap();
        }
        Env { _dir: dir, server }
    }

    /// `None` when the call did not finish in time (it got past authorization).
    async fn call(
        &self,
        ctx: &crate::api::Ctx,
        method: &str,
        p: Value,
    ) -> Option<Result<Value, RpcError>> {
        tokio::time::timeout(
            Duration::from_secs(2),
            dispatch(&self.server, ctx, method, &p),
        )
        .await
        .ok()
    }
}

fn denied(r: &Option<Result<Value, RpcError>>) -> bool {
    matches!(r, Some(Err(e)) if e.code == ErrorKind::PermissionDenied.code())
}

#[test]
fn handler_full_scope_methods_are_catalogued_forbidden() {
    for m in [
        "preview.mirror",
        "preview.unmirror",
        "preview.profile.reset",
    ] {
        assert_eq!(pane_scope_of(m), PaneScope::Forbidden, "{m}");
    }
    assert_eq!(pane_scope_of("assistant.generate"), PaneScope::Forbidden);
    assert_eq!(pane_scope_of("pane.send_text"), PaneScope::OwnTarget);
    assert_eq!(pane_scope_of("agent.start"), PaneScope::OwnTarget);
    assert_eq!(pane_scope_of("pane.read"), PaneScope::Open);
    assert_eq!(pane_scope_of("preview.profile"), PaneScope::Open);
}

#[tokio::test(flavor = "multi_thread")]
async fn forbidden_methods_are_refused_at_authorize() {
    let e = Env::new();
    let pane = ctx_pane();
    let mut checked = 0;
    for m in methods()
        .into_iter()
        .chain(crate::api::PANE_FORBIDDEN.iter().copied())
        .filter(|m| pane_scope_of(m) == PaneScope::Forbidden)
    {
        let r = crate::api::authorize(&e.server, &pane, m, &json!({}));
        assert!(
            matches!(&r, Err(x) if x.code == ErrorKind::PermissionDenied.code()),
            "{m}: pane scope must be refused at authorize, got {r:?}"
        );
        checked += 1;
    }
    assert!(checked > 60, "only {checked} forbidden methods checked");
    // And through dispatch, for the methods whose handlers used to be the only check.
    for m in [
        "preview.mirror",
        "preview.unmirror",
        "preview.profile.reset",
    ] {
        let r = e
            .call(&pane, m, json!({"preview": "web", "profile": "default"}))
            .await;
        assert!(denied(&r), "{m}: {r:?}");
        let msg = r.unwrap().unwrap_err().message;
        assert!(msg.contains("forbidden for pane scope"), "{m}: {msg}");
    }
}

/// For every `Open` method, a pane-scoped call with `{}` must not be refused with
/// `PermissionDenied` unless a full-scope call with `{}` is refused too (or the denial is a
/// listed conditional one). A full-scope call is only made when the pane call was refused.
#[tokio::test(flavor = "multi_thread")]
async fn open_methods_are_not_refused_to_panes() {
    let e = Env::new();
    let (pane, full) = (ctx_pane(), ctx_full());
    let mut miscatalogued = vec![];
    let mut seen_conditional = vec![];
    for m in methods() {
        if pane_scope_of(m) != PaneScope::Open {
            continue;
        }
        let r = e.call(&pane, m, json!({})).await;
        if !denied(&r) {
            continue;
        }
        if CONDITIONAL_DENIALS.iter().any(|(c, _)| *c == m) {
            seen_conditional.push(m);
            continue;
        }
        let rf = e.call(&full, m, json!({})).await;
        if !denied(&rf) {
            miscatalogued.push(format!("{m}: {:?}", r.unwrap().unwrap_err().message));
        }
    }
    assert!(
        miscatalogued.is_empty(),
        "catalogued `open` but refused to panes (add to api::PANE_FORBIDDEN or CONDITIONAL_DENIALS): {miscatalogued:#?}"
    );
    for (c, why) in CONDITIONAL_DENIALS {
        assert_eq!(pane_scope_of(c), PaneScope::Open, "{c} ({why})");
        assert!(
            seen_conditional.contains(c),
            "stale conditional denial {c} ({why})"
        );
    }
}
