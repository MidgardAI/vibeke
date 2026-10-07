//! The unified blob store (02 §1.1; 3D): uploads are ingested next to screenshots and payloads,
//! `blob.get/stat` read the one store, `blob.stats` / `blob.gc` maintain it.

use super::*;
use crate::api::dispatch;
use crate::core::Tx;
use crate::hardening::testkit::{pane_ctx, sample_pane, server, user};
use base64::Engine;
use std::sync::Arc;

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn setup(session: &str) -> (tempfile::TempDir, Arc<Server>) {
    let dir = tempfile::tempdir().unwrap();
    let s = server(dir.path(), session);
    let mut c = s.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.pane(sample_pane("pa", "wa"));
    tx.pane(sample_pane("pb", "wb"));
    s.commit(&mut c, tx).unwrap();
    drop(c);
    (dir, s)
}

fn unique(tag: &str) -> Vec<u8> {
    format!("{tag} {} {}", std::process::id(), vk_store::now_ms()).into_bytes()
}

#[tokio::test]
async fn blob_put_is_ingested_and_read_from_the_one_store() {
    let (_d, s) = setup("put");
    let data = unique("upload one");
    let put = dispatch(
        &s,
        &pane_ctx("pa"),
        "blob.put",
        &json!({"data_b64": b64(&data), "name": "note.txt", "mime": "text/plain"}),
    )
    .await
    .unwrap();
    let hash = put["hash"].as_str().unwrap().to_string();
    // The agent still gets its inbox path, and the file is there.
    let inbox_path = std::path::PathBuf::from(put["path"].as_str().unwrap());
    assert_eq!(std::fs::read(&inbox_path).unwrap(), data);
    // The store has its own copy with an `inbox` sidecar naming the uploader.
    let bs = store(&s);
    let info = bs.find(&hash).expect("ingested");
    assert_eq!(info.source(), "inbox");
    assert_eq!(info.meta.as_ref().unwrap()["pane"], "pa");
    assert_eq!(info.meta.as_ref().unwrap()["workspace"], "wa");
    assert_eq!(info.meta.as_ref().unwrap()["name"], "note.txt");
    assert_eq!(info.exts, ["txt"]);
    // blob.stat reads the store copy: one reference, not inbox plus store.
    let st = dispatch(&s, &user(), "blob.stat", &json!({"hash": hash}))
        .await
        .unwrap();
    assert_eq!(st["refs"], 1);
    assert_eq!(st["mime"], "text/plain");
    assert_eq!(st["size"], data.len());
    assert!(
        std::path::Path::new(st["path"].as_str().unwrap()).starts_with(s.paths.blobs()),
        "{st}"
    );
    // Reads stay scoped: the uploader and its workspace yes, another workspace no.
    for (ctx, ok) in [
        (pane_ctx("pa"), true),
        (user(), true),
        (pane_ctx("pb"), false),
    ] {
        let r = dispatch(&s, &ctx, "blob.get", &json!({"hash": hash})).await;
        assert_eq!(r.is_ok(), ok, "{:?}", ctx.pane_scope);
    }
    let got = dispatch(&s, &user(), "blob.get", &json!({"hash": hash}))
        .await
        .unwrap();
    assert_eq!(got["data_b64"], b64(&data));
}

#[tokio::test]
async fn the_same_bytes_uploaded_twice_make_one_blob() {
    let (_d, s) = setup("twice");
    let data = unique("same");
    for p in ["pa", "pa"] {
        dispatch(
            &s,
            &pane_ctx(p),
            "blob.put",
            &json!({"data_b64": b64(&data), "name": "same.bin"}),
        )
        .await
        .unwrap();
    }
    assert_eq!(store(&s).list().len(), 1);
}

#[tokio::test]
async fn chunked_uploads_are_ingested_too() {
    let (_d, s) = setup("chunked");
    let data = unique("chunked payload");
    let ctx = user();
    let begin = dispatch(
        &s,
        &ctx,
        "blob.begin",
        &json!({"name": "big.log", "size": data.len()}),
    )
    .await
    .unwrap();
    let id = begin["upload_id"].as_str().unwrap();
    dispatch(
        &s,
        &ctx,
        "blob.append",
        &json!({"upload_id": id, "offset": 0, "data_b64": b64(&data)}),
    )
    .await
    .unwrap();
    let done = dispatch(&s, &ctx, "blob.commit", &json!({"upload_id": id}))
        .await
        .unwrap();
    let hash = done["hash"].as_str().unwrap();
    assert_eq!(hash, blake3::hash(&data).to_hex().as_str());
    let info = store(&s).find(hash).expect("ingested");
    assert_eq!(info.source(), "inbox");
    assert_eq!(
        info.meta.as_ref().unwrap()["pane"],
        Value::Null,
        "full-scope uploader"
    );
    assert_eq!(info.exts, ["log"]);
}

#[tokio::test]
async fn browser_staged_drops_are_not_blobs() {
    let (_d, s) = setup("stage");
    let data = unique("for a page");
    let ctx = user();
    let begin = dispatch(
        &s,
        &ctx,
        "blob.begin",
        &json!({"name": "pic.png", "size": data.len()}),
    )
    .await
    .unwrap();
    let id = begin["upload_id"].as_str().unwrap();
    dispatch(
        &s,
        &ctx,
        "blob.append",
        &json!({"upload_id": id, "offset": 0, "data_b64": b64(&data)}),
    )
    .await
    .unwrap();
    let _ = dispatch(
        &s,
        &ctx,
        "blob.commit",
        &json!({"upload_id": id, "stage": "browser"}),
    )
    .await;
    assert!(
        store(&s).find(&blake3::hash(&data).to_hex()).is_none(),
        "a drop staged for a page is not ingested"
    );
}

#[tokio::test]
async fn ingest_never_overwrites_an_owner_sidecar() {
    let (_d, s) = setup("owner");
    let png = unique("png bytes");
    // A screenshot wrote this blob first.
    let (hash, _) = crate::agent_browser::store_blob(
        &s,
        &png,
        "png",
        &json!({"source": "screenshot", "pane": "pa", "workspace": "wa"}),
    )
    .unwrap();
    // The same bytes are uploaded: the sidecar stays the screenshot's.
    dispatch(
        &s,
        &pane_ctx("pa"),
        "blob.put",
        &json!({"data_b64": b64(&png), "name": "same.png"}),
    )
    .await
    .unwrap();
    let info = store(&s).find(&hash).unwrap();
    assert_eq!(info.source(), "screenshot");
    // And gc never takes a screenshot, however old and unreferenced.
    let r = gc(&s, 0, false);
    assert_eq!(r.removed, 0);
    assert_eq!(r.kept_uncollectable, 1);
    assert!(store(&s).find(&hash).is_some());
}

#[test]
fn legacy_inbox_uploads_are_adopted_once() {
    let (_d, s) = setup("legacy");
    let data = unique("legacy upload");
    let hash = blake3::hash(&data).to_hex().to_string();
    // As an older server left it: a file in the inbox and an owner record, no store copy.
    let dir = crate::paths::Paths::inbox().join(&hash[..12]);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("old.txt"), &data).unwrap();
    // A different file that happens to share the directory must not be adopted as this hash.
    std::fs::write(dir.join("stranger.txt"), b"someone else's file").unwrap();
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv(
            "blob_owner",
            &hash,
            Some(json!([{"pane": "pa", "workspace": "wa", "client": "c"}]).to_string()),
        );
        s.commit(&mut c, tx).unwrap();
    }
    assert!(store(&s).find(&hash).is_none());
    assert_eq!(adopt_legacy(&s), 1);
    let info = store(&s).find(&hash).unwrap();
    assert_eq!(info.source(), "inbox");
    assert_eq!(info.meta.as_ref().unwrap()["pane"], "pa");
    assert_eq!(info.meta.as_ref().unwrap()["adopted"], true);
    assert_eq!(store(&s).list().len(), 1, "the stranger was not ingested");
    // Idempotent.
    assert_eq!(adopt_legacy(&s), 0);
}

#[tokio::test]
async fn blob_stats_group_by_source() {
    let (_d, s) = setup("stats");
    put_payload(&s, b"payload one", "txt", "text/plain", Some("pa")).unwrap();
    crate::agent_browser::store_blob(&s, b"png", "png", &json!({"source": "screenshot"})).unwrap();
    dispatch(
        &s,
        &pane_ctx("pa"),
        "blob.put",
        &json!({"data_b64": b64(&unique("up")), "name": "u.bin"}),
    )
    .await
    .unwrap();
    let st = dispatch(&s, &user(), "blob.stats", &json!({}))
        .await
        .unwrap();
    assert_eq!(st["count"], 3);
    assert_eq!(st["by_source"]["payload"]["count"], 1);
    assert_eq!(st["by_source"]["screenshot"]["count"], 1);
    assert_eq!(st["by_source"]["inbox"]["count"], 1);
    assert!(st["bytes"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn blob_gc_dry_run_reports_and_age_validation() {
    let (_d, s) = setup("gcapi");
    let h = put_payload(&s, b"orphan payload", "txt", "text/plain", Some("pa")).unwrap();
    // Young blobs stay at the default age.
    let r = dispatch(&s, &user(), "blob.gc", &json!({})).await.unwrap();
    assert_eq!(
        (r["removed"].clone(), r["kept_young"].clone()),
        (json!(0), json!(1))
    );
    assert_eq!(r["older_than_days"], DEFAULT_GC_DAYS);
    // Age 0 + dry run: reported, not removed.
    let dry = dispatch(
        &s,
        &user(),
        "blob.gc",
        &json!({"older_than_days": 0, "dry_run": true}),
    )
    .await
    .unwrap();
    assert_eq!(dry["removed"], 1);
    assert_eq!(dry["hashes"][0], h);
    assert!(store(&s).find(&h).is_some());
    // For real.
    let real = dispatch(&s, &user(), "blob.gc", &json!({"older_than_days": 0}))
        .await
        .unwrap();
    assert_eq!(real["removed"], 1);
    assert!(store(&s).find(&h).is_none());
    // Bad ages are refused.
    for bad in [json!(-1), json!("soon"), json!(1.5)] {
        let e = dispatch(&s, &user(), "blob.gc", &json!({"older_than_days": bad}))
            .await
            .unwrap_err();
        assert_eq!(e.data.kind, "invalid_params");
    }
}

#[tokio::test]
async fn store_maintenance_is_full_scope_only() {
    let (_d, s) = setup("scope");
    for m in ["blob.stats", "blob.gc"] {
        assert_eq!(
            crate::api::pane_scope_of(m),
            crate::api::PaneScope::Forbidden
        );
        let e = dispatch(&s, &pane_ctx("pa"), m, &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.data.kind, "permission_denied", "{m}");
    }
}

#[test]
fn payloads_are_readable_by_the_owning_workspace_only() {
    let (_d, s) = setup("payload-scope");
    let h = put_payload(&s, b"tool output of pa", "txt", "text/plain", Some("pa")).unwrap();
    let info = store(&s).find(&h).unwrap();
    assert_eq!(info.meta.as_ref().unwrap()["workspace"], "wa");
    assert_eq!(info.source(), "payload");
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    rt.block_on(async {
        assert!(
            dispatch(&s, &pane_ctx("pa"), "blob.get", &json!({"hash": h}))
                .await
                .is_ok()
        );
        assert!(
            dispatch(&s, &pane_ctx("pb"), "blob.get", &json!({"hash": h}))
                .await
                .is_err()
        );
        assert!(
            dispatch(&s, &user(), "blob.get", &json!({"hash": h}))
                .await
                .is_ok()
        );
    });
}

/// Final review P1 9: pane A uploads text that pane B's run also holds as a tool-output payload
/// (one hash). Forgetting pane A removes A's upload but keeps B's payload readable.
#[tokio::test]
async fn forgetting_an_upload_keeps_a_payload_another_pane_references() {
    let (_d, s) = setup("fshare");
    let run = crate::hardening::testkit::sample_run("runb", "pb");
    {
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.run(run.clone());
        s.commit(&mut c, tx).unwrap();
    }
    let out = format!(
        "{}\n{}",
        String::from_utf8(unique("shared output")).unwrap(),
        "log line\n".repeat(400)
    );
    crate::items::observe_hook(&s, &run, "UserPromptSubmit", &json!({"prompt": "go"}));
    crate::items::observe_hook(
        &s,
        &run,
        "PostToolUse",
        &json!({"tool_name": "Bash", "tool_use_id": "t1", "tool_input": {"command": "cat log"}, "tool_response": out}),
    );
    let refs = crate::items::payload_refs(&s);
    assert_eq!(refs.len(), 1);
    let hash = refs.into_iter().next().unwrap();
    let ext = store(&s).find(&hash).unwrap().exts[0].clone();
    let bytes = store(&s).read(&hash, &ext).unwrap();
    // Pane A uploads the identical text.
    let put = dispatch(
        &s,
        &pane_ctx("pa"),
        "blob.put",
        &json!({"data_b64": b64(&bytes), "name": "same.txt"}),
    )
    .await
    .unwrap();
    assert_eq!(put["hash"], hash.as_str());
    // Forget pane A (dry run, then the confirmed plan).
    let d = dispatch(
        &s,
        &user(),
        "state.forget",
        &json!({"pane": "pa", "dry_run": true}),
    )
    .await
    .unwrap();
    let mut confirm = d["scope"].clone();
    confirm["plan"] = d["plan"].clone();
    let r = dispatch(&s, &user(), "state.forget", &confirm)
        .await
        .unwrap();
    assert_eq!(r["also"]["uploads"]["removed"], 1, "{r}");
    // B's payload stays readable.
    assert!(store(&s).read(&hash, &ext).unwrap() == bytes);
    let got = dispatch(&s, &pane_ctx("pb"), "blob.get", &json!({"hash": hash}))
        .await
        .unwrap();
    assert_eq!(got["data_b64"], b64(&bytes));
    // Once nothing references it any more, forgetting B removes it.
    let d = dispatch(
        &s,
        &user(),
        "state.forget",
        &json!({"pane": "pb", "dry_run": true}),
    )
    .await
    .unwrap();
    let mut confirm = d["scope"].clone();
    confirm["plan"] = d["plan"].clone();
    dispatch(&s, &user(), "state.forget", &confirm)
        .await
        .unwrap();
    assert!(crate::items::payload_refs(&s).is_empty());
}
