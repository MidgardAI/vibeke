use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::fake::FakeServer;
use crate::{Credential, MemoryStore};

/// An account signed in to `f` with the fake's current tokens.
fn signed_in(f: &FakeServer) -> Account {
    f.with(|s| {
        s.access = Some("a1".into());
        s.refresh = Some("r1".into());
        s.issued = 1;
    });
    let client = Client::new(&f.url).unwrap();
    let store = Arc::new(MemoryStore::default());
    let acct = Account::new(client, store);
    acct.save(&Credential {
        server: acct.server().to_string(),
        login: "octo".into(),
        refresh_token: "r1".into(),
        access_token: Some("a1".into()),
        access_exp: Some(crate::now_s() + 3600),
        saved_at: crate::now_s(),
    })
    .unwrap();
    acct
}

#[test]
fn access_on_the_wire() {
    assert_eq!(
        BlobAccess::Github(vec![1, 2]).to_json(),
        json!({"access": "github", "github": [1, 2]})
    );
    assert_eq!(BlobAccess::Any.to_json(), json!({"access": "any"}));
    assert_eq!(
        error_code(&Error::Http("HTTP 404: not_found".into())),
        Some("not_found")
    );
    assert_eq!(error_code(&Error::Http("HTTP 500".into())), None);
    assert_eq!(error_code(&Error::LoginRequired), None);
}

#[tokio::test]
async fn create_upload_meta_download_delete() {
    let f = FakeServer::start().await;
    let acct = signed_in(&f);
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("blob.bin");
    let data: Vec<u8> = (0..700_000u32).map(|i| (i % 253) as u8).collect();
    std::fs::write(&src, &data).unwrap();

    let c = acct
        .blob_create(data.len() as u64, Some(3600), BlobAccess::Any)
        .await
        .unwrap();
    assert!(c.expires_at > crate::now_s());
    let sent = Arc::new(AtomicU64::new(0));
    let s2 = sent.clone();
    acct.blob_upload_with(
        &c.id,
        &src,
        Some(Arc::new(move |n| s2.store(n, Ordering::SeqCst))),
    )
    .await
    .unwrap();
    assert_eq!(sent.load(Ordering::SeqCst), data.len() as u64);

    // A second upload is refused.
    let e = acct.blob_upload(&c.id, &src).await.unwrap_err();
    assert_eq!(error_code(&e), Some("already_uploaded"));

    let m = acct.blob_meta(&c.id).await.unwrap();
    assert_eq!(m.size, data.len() as u64);
    assert_eq!((m.from.id, m.from.login.as_str()), (4242, "octo"));

    let dest = t.path().join("down.bin");
    acct.blob_download(&c.id, &dest).await.unwrap();
    assert_eq!(std::fs::read(&dest).unwrap(), data);

    // More than allowed: refused and nothing left behind.
    let small = t.path().join("small.bin");
    let e = acct
        .blob_download_with(&c.id, &small, 1000, None)
        .await
        .unwrap_err();
    assert_eq!(error_code(&e), Some("too_large"));
    assert!(!small.exists());

    acct.blob_delete(&c.id).await.unwrap();
    let e = acct.blob_meta(&c.id).await.unwrap_err();
    assert_eq!(error_code(&e), Some("not_found"));
    let e = acct
        .blob_download(&c.id, &t.path().join("gone.bin"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&e), Some("not_found"));
}

#[tokio::test]
async fn refusals() {
    let f = FakeServer::start().await;
    let acct = signed_in(&f);
    let t = tempfile::tempdir().unwrap();
    let src = t.path().join("blob.bin");
    std::fs::write(&src, b"0123456789").unwrap();

    // The declared size must match the upload.
    let c = acct.blob_create(5, None, BlobAccess::Any).await.unwrap();
    let e = acct.blob_upload(&c.id, &src).await.unwrap_err();
    assert_eq!(error_code(&e), Some("size_mismatch"));
    // Not uploaded yet.
    let e = acct
        .blob_download(&c.id, &t.path().join("x"))
        .await
        .unwrap_err();
    assert_eq!(error_code(&e), Some("not_uploaded"));

    // Someone else's GitHub id only.
    let c = acct
        .blob_create(10, None, BlobAccess::Github(vec![1]))
        .await
        .unwrap();
    acct.blob_upload(&c.id, &src).await.unwrap();
    let e = acct.blob_meta(&c.id).await.unwrap_err();
    assert_eq!(error_code(&e), Some("forbidden"));

    // Too large for the server.
    f.blobs.with(|s| s.max_size = 4);
    let e = acct
        .blob_create(5, None, BlobAccess::Any)
        .await
        .unwrap_err();
    assert_eq!(error_code(&e), Some("too_large"));

    // A bad id never reaches a URL.
    assert!(matches!(
        acct.blob_meta("../x").await.unwrap_err(),
        Error::Protocol(_)
    ));

    // Signed out on the server: the refresh fails too → login required.
    f.with(|s| {
        s.access = None;
        s.refresh = None;
    });
    let e = acct.blob_meta("blob1").await.unwrap_err();
    assert_eq!(e, Error::LoginRequired);
}
