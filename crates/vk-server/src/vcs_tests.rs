//! `Pane.jj` refresh against a fake `jj` (no real jj needed).

use super::*;
use crate::ServerOpts;
use crate::core::Tx;
use crate::paths::Paths;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use vk_proto::model::Pane;

fn server_at(root: &Path) -> Arc<Server> {
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
    Server::new(paths, opts).unwrap()
}

fn put_pane(server: &Arc<Server>, id: &str, cwd: &Path) {
    let p = Pane {
        id: id.into(),
        handle: id.into(),
        tab: "tab".into(),
        workspace: "ws".into(),
        title: None,
        auto_title: String::new(),
        cwd: Some(cwd.to_string_lossy().into_owned()),
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
        isolation: Default::default(),
        recovered: None,
        browser: None,
        jj: None,
    };
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.pane(p);
    server.commit(&mut c, tx).unwrap();
}

fn jj_of(server: &Arc<Server>, id: &str) -> Option<String> {
    server.with_core(|c| c.pane(id).and_then(|p| p.jj.clone()))
}

#[tokio::test]
async fn refresh_sets_and_clears_the_jj_label() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let server = server_at(&root);
    let jj_repo = root.join("repo");
    let plain = root.join("plain");
    std::fs::create_dir_all(jj_repo.join(".jj")).unwrap();
    std::fs::create_dir_all(jj_repo.join("src")).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    put_pane(&server, "a", &jj_repo.join("src"));
    put_pane(&server, "b", &plain);
    let bin = root.join("jj");
    std::fs::write(&bin, "#!/bin/sh\nprintf 'main'\n").unwrap();
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();

    refresh_with(&server, vec!["a".into(), "b".into()], bin.clone()).await;
    assert_eq!(jj_of(&server, "a").as_deref(), Some("main"));
    assert_eq!(jj_of(&server, "b"), None, "not a jj repo");

    // The change moves: the next refresh picks it up.
    std::fs::write(&bin, "#!/bin/sh\nprintf 'kxyzabcd'\n").unwrap();
    refresh_with(&server, vec!["a".into()], bin.clone()).await;
    assert_eq!(jj_of(&server, "a").as_deref(), Some("kxyzabcd"));

    // `jj` gone (not installed): nothing shown.
    refresh_with(&server, vec!["a".into()], root.join("no-such-jj")).await;
    assert_eq!(jj_of(&server, "a"), None);
}
