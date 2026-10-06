//! Blob ownership (review batch 2, finding 5): the inbox is shared by every session of an
//! installation, but `blob.get` / `blob.stat` only find blobs the calling session owns, and a
//! pane only its own or its workspace's. Throwaway servers; the installation's state root (the
//! shared inbox) is a temp dir.

use super::*;
use crate::api::dispatch;
use crate::core::Tx;
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use std::sync::{Arc, Once};
use vk_proto::model::Pane;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-blob-tests-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        // SAFETY: set once, before any server in this test binary reads them; never the
        // user's real state directory.
        unsafe {
            if std::env::var_os("VIBEKE_STATE_DIR").is_none() {
                std::env::set_var("VIBEKE_STATE_DIR", base.join("state"));
            }
            if std::env::var_os("VIBEKE_CONFIG").is_none() {
                std::env::set_var("VIBEKE_CONFIG", base.join("config.toml"));
            }
        }
    });
}

fn server(dir: &std::path::Path, session: &str) -> Arc<Server> {
    let root = dir.canonicalize().unwrap();
    let paths = Paths {
        session: session.into(),
        runtime: root.join("run").join(session),
        state: root.join("state").join(session),
    };
    let opts = ServerOpts {
        session: session.into(),
        machine: "m".into(),
        bin: "/bin/false".into(),
        hold_args: vec![],
        default_shell: None,
        env: vec![],
        shims: false,
    };
    Server::new(paths, opts).unwrap()
}

fn pane(id: &str, ws: &str) -> Pane {
    Pane {
        id: id.into(),
        handle: id.into(),
        tab: "tab".into(),
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
        browser: None,
    }
}

fn put_pane(server: &Server, p: Pane) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.pane(p);
    server.commit(&mut c, tx).unwrap();
}

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

fn pane_ctx(p: &str) -> Ctx {
    Ctx {
        client_id: format!("c-{p}"),
        kind: "cli".into(),
        pane_scope: Some(p.into()),
        remote: false,
    }
}

async fn readable(server: &Arc<Server>, ctx: &Ctx, hash: &str) -> bool {
    let get = dispatch(server, ctx, "blob.get", &json!({"hash": hash})).await;
    let stat = dispatch(server, ctx, "blob.stat", &json!({"hash": hash})).await;
    assert_eq!(get.is_ok(), stat.is_ok(), "get and stat agree");
    if let Err(e) = &get {
        assert_eq!(e.data.kind, "not_found", "{e:?}");
    }
    get.is_ok()
}

#[tokio::test]
async fn blob_reads_are_scoped_to_the_owning_session_and_pane() {
    init_env();
    let dir = tempfile::tempdir().unwrap();
    let a = server(dir.path(), "a");
    let b = server(dir.path(), "b");
    put_pane(&a, pane("pa", "wa"));
    for (p, ws) in [("pb", "wb"), ("pb2", "wb"), ("pc", "wc")] {
        put_pane(&b, pane(p, ws));
    }
    // Uploaded only in B, by pane pb.
    let data = format!("secret for b only {}", std::process::id());
    let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &data);
    let put = dispatch(
        &b,
        &pane_ctx("pb"),
        "blob.put",
        &json!({"data_b64": b64, "name": "b.txt"}),
    )
    .await
    .unwrap();
    let hash = put["hash"].as_str().unwrap().to_string();
    // The file is in the shared inbox, but A owns nothing: neither its pane nor its user can
    // read it through A's server, by hash.
    assert!(Paths::inbox().join(&hash[..12]).join("b.txt").is_file());
    assert!(!readable(&a, &pane_ctx("pa"), &hash).await, "A's pane");
    assert!(!readable(&a, &user(), &hash).await, "A's user");
    // In B: the uploader, its workspace and B's user can; another workspace's pane can't.
    assert!(
        readable(&b, &pane_ctx("pb"), &hash).await,
        "the uploading pane"
    );
    assert!(
        readable(&b, &pane_ctx("pb2"), &hash).await,
        "same workspace"
    );
    assert!(readable(&b, &user(), &hash).await, "B's user");
    assert!(
        !readable(&b, &pane_ctx("pc"), &hash).await,
        "another workspace"
    );
    // Once A's own user uploads the same content, A owns it (for its user, not B's panes).
    dispatch(
        &a,
        &user(),
        "blob.put",
        &json!({"data_b64": b64, "name": "b.txt"}),
    )
    .await
    .unwrap();
    assert!(readable(&a, &user(), &hash).await);
    assert!(
        !readable(&a, &pane_ctx("pa"), &hash).await,
        "a user upload is not the pane's"
    );
}
