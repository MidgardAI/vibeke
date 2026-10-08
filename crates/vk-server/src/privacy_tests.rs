//! In-process tests of lane 3E (09 §9.1–9.3): state encryption with a fake (file) keychain,
//! sealed blobs and segments, index and search redaction, `state.forget` coverage and the
//! encryption API. A throwaway server with no holders; settings are forced on the server, so
//! no config file or real keychain is involved.

use super::*;
use crate::api::{Ctx, dispatch};
use crate::core::Tx;
use crate::paths::Paths;
use crate::{Server, ServerOpts};
use std::sync::Once;
use vk_store::archive::ArchivedRow;

fn init_env() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let base = std::env::temp_dir().join(format!("vk-privacy-tests-{}", std::process::id()));
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

fn user() -> Ctx {
    Ctx {
        client_id: "c-user".into(),
        kind: "cli".into(),
        pane_scope: None,
        remote: false,
    }
}

struct Env {
    dir: tempfile::TempDir,
    server: Arc<Server>,
}

const SECRET: &str = "ghp_abcdefghijklmnopqrstuvwxyz0123456789";

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
            gateway: None,
        };
        let server = Server::new(paths, opts).unwrap();
        Env { dir, server }
    }

    fn keychain(&self) -> Keychain {
        Keychain::File(self.dir.path().join("fake-keychain.json"))
    }

    fn force(&self, s: Settings) {
        *self.server.privacy.forced.lock().unwrap() = Some(s.clone());
        apply(&self.server, &s);
    }

    fn encrypt(&self) {
        self.force(Settings {
            encrypt_state: true,
            keychain: Ok(self.keychain()),
            ..Default::default()
        });
    }

    fn archive(&self, pane: &str, from: u64, text: &str) {
        let rows = (from..from + 5)
            .map(|n| ArchivedRow {
                n,
                t: format!("{text} {n}"),
                w: false,
            })
            .collect();
        self.server.archive_rows(pane, rows);
        self.server.housekeeping();
    }

    async fn call(&self, method: &str, p: Value) -> R {
        dispatch(&self.server, &user(), method, &p).await
    }
}

#[test]
fn settings_parse_from_config() {
    let cfg: vk_config::Config = vk_config::Config::parse(
        "[security]\nencrypt_state = true\nkeychain = \"file:/tmp/kc.json\"\nredact_scrollback_index = false\n[security.redact]\npatterns = [\"corp-[0-9]+\"]\n",
        std::path::Path::new("config.toml"),
    )
    .map(|(c, _)| c)
    .unwrap();
    let s = Settings::from_config(&cfg);
    assert!(s.encrypt_state);
    assert_eq!(s.keychain, Ok(Keychain::File("/tmp/kc.json".into())));
    assert!(!s.redact_scrollback_index);
    assert_eq!(s.patterns, vec!["corp-[0-9]+".to_string()]);
    let d = Settings::from_config(&vk_config::Config::default());
    assert!(!d.encrypt_state && d.redact_scrollback_index);
    assert_eq!(d.keychain, Ok(Keychain::Os));
}

#[tokio::test]
async fn encryption_seals_new_segments_and_blobs_with_a_fake_keychain() {
    let e = Env::new();
    // Off by default: plain segment, nothing unlocked, no keychain touched.
    e.archive("p1", 0, "plainline");
    assert!(!status(&e.server).active);
    assert!(!e.keychain_path_exists());
    e.encrypt();
    let st = status(&e.server);
    assert!(st.active, "{st:?}");
    assert!(e.keychain_path_exists());
    e.archive("p1", 5, "SEALEDTEXT");
    let segs = e.server.archive.lock().unwrap().segment_infos("p1");
    assert_eq!(segs.len(), 2);
    assert!(!crypt::file_is_sealed(&segs[0].path));
    assert!(crypt::file_is_sealed(&segs[1].path));
    let raw = std::fs::read(&segs[1].path).unwrap();
    assert!(!raw.windows(10).any(|w| w == b"SEALEDTEXT"));
    // pane.read-style paging reads both modes.
    let rows = e
        .server
        .archive
        .lock()
        .unwrap()
        .read("p1", 0, u64::MAX)
        .unwrap();
    assert_eq!(rows.len(), 10);
    assert_eq!(rows[7].t, "SEALEDTEXT 7");
    // Blobs: sealed on disk, plaintext through blob.get, a decrypted view for tools.
    let png = b"\x89PNG fake image bytes".to_vec();
    let (hash, path) = crate::agent_browser::store_blob(
        &e.server,
        &png,
        "png",
        &json!({"kind": "pane_screenshot", "created_at_ms": 1}),
    )
    .unwrap();
    assert!(crypt::file_is_sealed(&path));
    let got = e.call("blob.get", json!({"hash": hash})).await.unwrap();
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD
        .decode(got["data_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(data, png);
    assert_eq!(got["size"], png.len());
    let ranged = e
        .call(
            "blob.get",
            json!({"hash": hash, "range": {"offset": 1, "length": 3}}),
        )
        .await
        .unwrap();
    assert_eq!(ranged["length"], 3);
    let view = readable_path(&e.server, &path);
    assert_ne!(view, path);
    assert!(view.starts_with(&e.server.paths.runtime));
    assert_eq!(std::fs::read(&view).unwrap(), png);
    assert_eq!(prune_plain_views(&e.server, Duration::ZERO), 1);
    // Status reports both modes; migrate seals the old plain segment, then back to plain.
    let s = e
        .call("security.encryption.status", json!({}))
        .await
        .unwrap();
    assert_eq!(s["active"], true);
    assert_eq!(s["files"]["plain"], 1);
    let dry = e
        .call(
            "security.encryption.migrate",
            json!({"to": "sealed", "dry_run": true}),
        )
        .await
        .unwrap();
    assert_eq!(dry["changed"], 1);
    let m = e
        .call("security.encryption.migrate", json!({"to": "sealed"}))
        .await
        .unwrap();
    assert_eq!(m["changed"], 1, "{m}");
    assert!(crypt::file_is_sealed(&segs[0].path));
    e.call("security.encryption.migrate", json!({"to": "plain"}))
        .await
        .unwrap();
    assert!(!crypt::file_is_sealed(&segs[1].path));
    assert!(!crypt::file_is_sealed(&path));
    // Turning encryption off keeps the key for reading; new segments are plain again.
    e.force(Settings::default());
    let st = status(&e.server);
    assert!(!st.active && st.readable, "{st:?}");
}

impl Env {
    fn keychain_path_exists(&self) -> bool {
        self.dir.path().join("fake-keychain.json").exists()
    }
}

/// Final review P1 6: with `encrypt_state = true` and a keychain that fails to unlock, no
/// scrollback segment, index row or blob is written in plaintext; the status says writes are
/// paused; and the unlock is retried with unchanged settings, after which writes are sealed.
#[tokio::test]
async fn failed_unlock_persists_nothing_in_plaintext_and_retries() {
    let e = Env::new();
    // A keychain file whose parent is a regular file: creating the key fails.
    let blocker = e.dir.path().join("blocker");
    std::fs::write(&blocker, "not a dir").unwrap();
    let s = Settings {
        encrypt_state: true,
        keychain: Ok(Keychain::File(blocker.join("kc.json"))),
        ..Default::default()
    };
    e.force(s.clone());
    let st = status(&e.server);
    assert!(st.requested && !st.active && st.writes_paused, "{st:?}");
    assert!(st.error.is_some());
    // Scrollback: nothing archived, nothing indexed.
    e.archive("p1", 0, "PLAINLEAK");
    assert!(
        e.server
            .archive
            .lock()
            .unwrap()
            .segment_infos("p1")
            .is_empty(),
        "a segment was written while locked"
    );
    let hits = e
        .server
        .with_core(|c| c.store.fts_search("PLAINLEAK", None, 10))
        .unwrap_or_default();
    assert!(hits.is_empty(), "indexed while locked");
    let scrollback = e.server.paths.state.join("scrollback");
    let leaked = walk_contains(&scrollback, b"PLAINLEAK");
    assert!(!leaked, "plaintext scrollback on disk");
    // Blobs: every write path refuses.
    let r = crate::agent_browser::store_blob(
        &e.server,
        b"PLAINBLOB bytes",
        "png",
        &json!({"kind": "pane_screenshot"}),
    );
    assert!(r.is_err(), "blob written while locked");
    let src = e.dir.path().join("upload.txt");
    std::fs::write(&src, "PLAINBLOB upload").unwrap();
    assert!(
        crate::blob_store::store(&e.server)
            .put_file(&src, "txt", &json!({}), vk_store::blobs::MetaMode::Replace)
            .is_err()
    );
    assert!(!walk_contains(&e.server.paths.blobs(), b"PLAINBLOB"));
    let api = e
        .call("security.encryption.status", json!({}))
        .await
        .unwrap();
    assert_eq!(api["writes_paused"], true);
    // The cached settings don't block the retry: the same settings unlock once the keychain
    // works, and writes resume sealed.
    std::fs::remove_file(&blocker).unwrap();
    apply(&e.server, &s);
    let st = status(&e.server);
    assert!(st.active && !st.writes_paused, "{st:?}");
    e.archive("p1", 5, "SEALEDNOW");
    let segs = e.server.archive.lock().unwrap().segment_infos("p1");
    assert_eq!(segs.len(), 1);
    assert!(crypt::file_is_sealed(&segs[0].path));
    let (_, path) = crate::agent_browser::store_blob(
        &e.server,
        b"PLAINBLOB later",
        "png",
        &json!({"kind": "pane_screenshot"}),
    )
    .unwrap();
    assert!(crypt::file_is_sealed(&path));
}

/// Final review P1 7 (server side): a large sealed blob is written chunked, `blob.get` ranges
/// decrypt only the chunks they touch, and migration works both ways.
#[tokio::test]
async fn large_sealed_blobs_read_in_ranges_and_migrate() {
    use base64::Engine as _;
    let e = Env::new();
    e.encrypt();
    let n = crypt::CHUNKED_ABOVE + 3 * crypt::CHUNK + 123;
    let data: Vec<u8> = (0..n).map(|i| (i.wrapping_mul(7) % 253) as u8).collect();
    let (hash, path) = crate::agent_browser::store_blob(
        &e.server,
        &data,
        "bin",
        &json!({"kind": "pane_screenshot", "created_at_ms": 1}),
    )
    .unwrap();
    assert!(std::fs::read(&path).unwrap().starts_with(crypt::MAGIC2));
    let get = |off: usize, len: usize| {
        let e = &e;
        let hash = hash.clone();
        async move {
            let r = e
                .call(
                    "blob.get",
                    json!({"hash": hash, "range": {"offset": off, "length": len}}),
                )
                .await?;
            Ok::<_, vk_proto::rpc::RpcError>(
                base64::engine::general_purpose::STANDARD
                    .decode(r["data_b64"].as_str().unwrap())
                    .unwrap(),
            )
        }
    };
    let off = crypt::CHUNK * 2 - 10;
    assert_eq!(get(off, 20).await.unwrap(), &data[off..off + 20]);
    assert_eq!(get(n - 5, 50).await.unwrap(), &data[n - 5..]);
    let st = e.call("blob.stat", json!({"hash": hash})).await.unwrap();
    assert_eq!(st["size"], n);
    // Damage chunk 0: a range in a later chunk still reads; one in chunk 0 is refused.
    let mut raw = std::fs::read(&path).unwrap();
    let good = raw.clone();
    raw[40] ^= 1;
    std::fs::write(&path, &raw).unwrap();
    let late = crypt::CHUNK * 6 + 3;
    assert_eq!(get(late, 64).await.unwrap(), &data[late..late + 64]);
    assert!(get(1, 4).await.is_err());
    std::fs::write(&path, &good).unwrap();
    // Migration to plain and back to sealed keeps the bytes.
    e.call("security.encryption.migrate", json!({"to": "plain"}))
        .await
        .unwrap();
    assert!(!crypt::file_is_sealed(&path));
    assert!(std::fs::read(&path).unwrap() == data);
    e.call("security.encryption.migrate", json!({"to": "sealed"}))
        .await
        .unwrap();
    assert!(crypt::file_is_sealed(&path));
    assert!(crypt::read_file(&path).unwrap() == data);
    assert_eq!(get(late, 64).await.unwrap(), &data[late..late + 64]);
}

fn walk_contains(dir: &std::path::Path, needle: &[u8]) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten().any(|e| {
        let p = e.path();
        if p.is_dir() {
            walk_contains(&p, needle)
        } else {
            std::fs::read(&p).is_ok_and(|b| b.windows(needle.len()).any(|w| w == needle))
        }
    })
}

#[tokio::test]
async fn encryption_requested_without_a_usable_keychain_stays_off_and_says_why() {
    let e = Env::new();
    e.force(Settings {
        encrypt_state: true,
        keychain: Err("keychain must be \"os\" or \"file:<path>\"".into()),
        ..Default::default()
    });
    let st = status(&e.server);
    assert!(st.requested && !st.active);
    assert!(st.error.unwrap().contains("keychain"));
    // A pane token can't see or change encryption.
    let pane = Ctx {
        client_id: "c-p".into(),
        kind: "cli".into(),
        pane_scope: Some("p1".into()),
        remote: false,
    };
    for m in [
        "security.encryption.status",
        "security.encryption.migrate",
        "state.forget",
    ] {
        assert_eq!(
            crate::api::pane_scope_of(m),
            crate::api::PaneScope::Forbidden
        );
        let r = dispatch(&e.server, &pane, m, &json!({})).await;
        assert!(r.is_err(), "{m}");
    }
}

#[tokio::test]
async fn index_and_search_are_redacted_but_paging_is_not() {
    let e = Env::new();
    e.archive("p1", 0, &format!("export GITHUB_TOKEN={SECRET} done"));
    // The index holds the redacted text (default on).
    let hits = e
        .server
        .with_core(|c| c.store.fts_search("GITHUB_TOKEN", Some("p1"), 10).unwrap());
    assert!(!hits.is_empty());
    assert!(hits.iter().all(|h| !h.3.contains(SECRET)));
    // Searching for the raw secret finds nothing in the index.
    let raw = e
        .server
        .with_core(|c| c.store.fts_search(SECRET, Some("p1"), 10).unwrap());
    assert!(raw.is_empty());
    // search.query results are redacted, including regex scans of the segments.
    let r = e
        .call(
            "search.query",
            json!({"q": "ghp_", "regex": true, "pane": "p1"}),
        )
        .await;
    if let Ok(v) = r {
        assert!(!v.to_string().contains(SECRET), "{v}");
    }
    // The segments (operational data) keep what the terminal showed.
    let rows = e.server.archive.lock().unwrap().read("p1", 0, 10).unwrap();
    assert!(rows[0].t.contains(SECRET));
    // With redaction off the index stores raw text, but remote clients still get redacted hits.
    e.force(Settings {
        redact_scrollback_index: false,
        ..Default::default()
    });
    e.archive("p2", 0, &format!("token {SECRET}"));
    let raw = e
        .server
        .with_core(|c| c.store.fts_search("token", Some("p2"), 10).unwrap());
    assert!(raw.iter().any(|h| h.3.contains(SECRET)));
    let mut v = json!({"hits": [{"text": format!("x {SECRET}"), "context": {"before": [SECRET], "after": []}}]});
    let remote = Ctx {
        remote: true,
        ..user()
    };
    redact_search(&e.server, &remote, &mut v);
    assert!(!v.to_string().contains(SECRET));
    assert_eq!(v["redacted"], true);
    let mut local = json!({"hits": [{"text": SECRET}]});
    redact_search(&e.server, &user(), &mut local);
    assert!(local.to_string().contains(SECRET));
}

#[test]
fn rebuild_transform_follows_the_setting() {
    let on = vk_config::Config::default();
    let f = index_transform(&on);
    assert!(!f(&format!("a {SECRET}")).contains(SECRET));
    let off = vk_config::Config::parse(
        "[security]\nredact_scrollback_index = false\n",
        std::path::Path::new("config.toml"),
    )
    .map(|(c, _)| c)
    .unwrap();
    let g = index_transform(&off);
    assert!(g(&format!("a {SECRET}")).contains(SECRET));
}

fn put_pane(e: &Env, id: &str, ws: &str) {
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.event(
        "pane.output_marker",
        json!({"pane": id, "workspace": ws}),
        json!({"text": "PANE-SECRET"}),
    );
    e.server.commit(&mut c, tx).unwrap();
    let _ = c.store.fts_register_panes(&[(
        id.into(),
        ws.into(),
        "t".into(),
        format!("{ws}:{id}"),
        "title".into(),
    )]);
}

fn put_draft(e: &Env, id: &str, ws: &str) {
    let mut c = e.server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put(
        crate::drafts::K_DRAFT,
        id,
        None,
        &json!({
            "id": id, "scope": "workspace", "scope_id": ws, "workspace": ws, "title": null,
            "text": "DRAFT-SECRET", "attachments": [], "order": 1.0, "rev": 1,
            "created_at_ms": 1, "updated_at_ms": 1
        }),
    );
    tx.m.put(
        crate::drafts::K_NOTES,
        ws,
        None,
        &json!({"workspace": ws, "text": "NOTES-SECRET", "rev": 1, "updated_at_ms": 1}),
    );
    e.server.commit(&mut c, tx).unwrap();
}

#[tokio::test]
async fn state_forget_covers_events_blobs_drafts_and_notes() {
    let e = Env::new();
    e.archive("p1", 0, "pane one text");
    e.archive("p2", 0, "pane two text");
    put_pane(&e, "p1", "w1");
    put_pane(&e, "p2", "w2");
    put_draft(&e, "d1", "w1");
    put_draft(&e, "d2", "w2");
    let (h1, path1) = crate::agent_browser::store_blob(
        &e.server,
        b"blob of p1",
        "png",
        &json!({"kind": "pane_screenshot", "pane": "p1", "created_at_ms": 5}),
    )
    .unwrap();
    let (_h2, path2) = crate::agent_browser::store_blob(
        &e.server,
        b"blob of p2",
        "png",
        &json!({"kind": "pane_screenshot", "pane": "p2", "created_at_ms": 5}),
    )
    .unwrap();
    // Dry run counts and changes nothing.
    let d = e
        .call("state.forget", json!({"workspace": "w1", "dry_run": true}))
        .await
        .unwrap();
    assert_eq!(d["also"]["blobs"]["files"], 1, "{d}");
    assert_eq!(d["also"]["drafts"]["drafts"], 1);
    assert_eq!(d["also"]["drafts"]["notes"], 1);
    assert!(d["also"]["events_tombstoned"].as_u64().unwrap() >= 1);
    assert!(path1.exists());
    // The real thing, through the plan the dry run returned.
    let mut confirm = d["scope"].clone();
    confirm["plan"] = d["plan"].clone();
    let r = e.call("state.forget", confirm).await.unwrap();
    assert_eq!(r["segments_deleted"], 1, "{r}");
    assert!(!path1.exists());
    assert!(path2.exists());
    assert!(e.call("blob.stat", json!({"hash": h1})).await.is_err());
    let drafts: Vec<Value> = e
        .server
        .with_core(|c| c.store.load::<Value>(crate::drafts::K_DRAFT).unwrap());
    assert_eq!(drafts.len(), 1);
    assert_eq!(drafts[0]["id"], "d2");
    // Events of w1/p1 became tombstones; w2's stay; seq stays gapless.
    let evs = e
        .server
        .with_core(|c| c.store.events_after(0, 10_000, &[]).unwrap());
    let text = serde_json::to_string(&evs).unwrap();
    assert!(!evs.iter().any(|ev| ev.subject["pane"] == "p1"));
    assert!(evs.iter().any(|ev| ev.subject["pane"] == "p2"));
    assert!(evs.iter().any(|ev| ev.kind == "tombstone"));
    assert!(evs.iter().any(|ev| ev.kind == "state.forgotten"));
    assert!(!text.contains("DRAFT-SECRET"));
    for w in evs.windows(2) {
        assert_eq!(w[1].seq, w[0].seq + 1);
    }
    // A stale plan is refused before anything is deleted.
    let stale = e
        .call(
            "state.forget",
            json!({"workspace": "w2", "plan": "fp1-stale"}),
        )
        .await;
    assert!(stale.is_err());
    assert!(path2.exists());
    // scrollback_only keeps the old behaviour.
    let s = e
        .call(
            "state.forget",
            json!({"pane": "p2", "scrollback_only": true}),
        )
        .await
        .unwrap();
    assert_eq!(s["scrollback_only"], true);
    assert!(s.get("also").is_none());
    assert!(path2.exists());
    // all: the rest goes.
    let a = e.call("state.forget", json!({"all": true})).await.unwrap();
    assert!(a["also"]["drafts"]["drafts"].as_u64().unwrap() >= 1, "{a}");
    assert!(!path2.exists());
}

#[tokio::test]
async fn state_forget_pane_scope_leaves_workspace_objects() {
    let e = Env::new();
    e.archive("p1", 0, "text");
    put_pane(&e, "p1", "w1");
    put_draft(&e, "d1", "w1");
    let r = e.call("state.forget", json!({"pane": "p1"})).await.unwrap();
    assert_eq!(r["also"]["drafts"]["drafts"], 0);
    assert!(
        r["not_covered"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x.as_str().unwrap().contains("drafts"))
    );
    let drafts: Vec<Value> = e
        .server
        .with_core(|c| c.store.load::<Value>(crate::drafts::K_DRAFT).unwrap());
    assert_eq!(drafts.len(), 1);
}
