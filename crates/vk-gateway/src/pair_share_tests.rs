//! Multi-use share invitations and GitHub attestations (spec 20 §2.3–§2.4, §9): use counts,
//! expiry, idempotent retries, concurrent claims, reconcile after a crash between the writes,
//! revocation tombstones and every attestation check.

use super::*;
use crate::state::{Config, Limit, Review};
use vk_account::fake::{ATTEST_SEED, FakeServer};

fn gateway(t: &tempfile::TempDir, account_url: Option<&str>) -> Arc<Gateway> {
    let st = StateDir::open(t.path().join("gw")).unwrap();
    st.save_config(&Config {
        relay: Some("ws://relay.test".into()),
        app_url: Some("https://app.test".into()),
        account_url: account_url.map(str::to_string),
        host_name: Some("devbox".into()),
        ..Config::default()
    })
    .unwrap();
    Gateway::new(st, crate::server::Server::new(t.path().join("none.sock"))).unwrap()
}

fn spec(github: bool, review: bool) -> ShareSpec {
    ShareSpec {
        kind: if review { "review" } else { "share" }.into(),
        ttl_s: 3600,
        until: now_s() + 3600,
        limit: Some(Limit {
            workspace: (!review).then(|| "w1".into()),
            pane: None,
            preview: review.then(|| "pv1".into()),
        }),
        label: None,
        owner: None,
        access: github.then(|| "github".into()),
        github: vec![GithubId {
            id: 42,
            login: "alice".into(),
        }],
        github_any: false,
        review: review.then(|| Review {
            preview: "pv1".into(),
            sid: "abcdefghijklmnop".into(),
            notes: "direct".into(),
            ..Default::default()
        }),
    }
}

fn invite(gw: &Gateway, sp: ShareSpec, uses: u32) -> Pairing {
    let redeemable = sp.redeemable();
    let (p, link) = create_invitation(
        &gw.state,
        "ws://relay.test",
        "devbox",
        Scope::View,
        true,
        Duration::from_secs(3600),
        Some(sp),
        uses,
    )
    .unwrap();
    let info = link.share_info().unwrap();
    assert_eq!(info.uses, redeemable.then_some(uses));
    p
}

fn alice() -> Option<GithubId> {
    Some(GithubId {
        id: 42,
        login: "alice".into(),
    })
}

fn kind<T>(r: Result<T, ApiError>) -> String {
    r.err().map(|e| e.kind).unwrap_or_else(|| "ok".into())
}

#[tokio::test(flavor = "multi_thread")]
async fn uses_are_counted_and_retries_are_idempotent() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let p = invite(&gw, spec(true, false), 2);
    let (a, new) = redeem(&gw, &p.pid, &[1; 32], "phone", "web", alice()).unwrap();
    assert!(new);
    assert_eq!(a.kind, "share");
    assert_eq!(a.invitation.as_deref(), Some(p.pid.as_str()));
    assert_eq!(a.github, alice());
    assert_eq!(a.name, "@alice");
    assert_eq!(a.scope, Scope::View);
    // The same key again: the same device, no second one, and no attestation needed.
    let (again, new) = redeem(&gw, &p.pid, &[1; 32], "phone", "web", None).unwrap();
    assert!(!new);
    assert_eq!(again.id, a.id);
    assert_eq!(gw.devices().len(), 1);
    let (b, _) = redeem(&gw, &p.pid, &[2; 32], "laptop", "web", alice()).unwrap();
    assert_ne!(b.id, a.id);
    // Exhausted: a new key is refused, the earlier ones still get their devices back.
    assert_eq!(
        kind(redeem(&gw, &p.pid, &[3; 32], "x", "web", alice())),
        "used"
    );
    let stored = gw.state.pairing(&p.pid).unwrap().unwrap();
    assert!(matches!(stored.status, PairingStatus::Done { .. }));
    assert_eq!(stored.redemptions.len(), 2);
    assert!(stored.admits_handshake(now_s()));
    assert!(stored.redemption_by(&b64::encode([2u8; 32])).is_some());
    assert!(stored.redemption_by(&b64::encode([3u8; 32])).is_none());
    assert_eq!(
        redeem(&gw, &p.pid, &[2; 32], "laptop", "web", None)
            .unwrap()
            .0
            .id,
        b.id
    );
    // A GitHub share never makes a device without an attestation.
    let q = invite(&gw, spec(true, false), 5);
    assert_eq!(
        kind(redeem(&gw, &q.pid, &[4; 32], "x", "web", None)),
        "upgrade_required"
    );
    // A key that is already one of this host's devices is not taken over.
    assert_eq!(
        kind(redeem(&gw, &q.pid, &[1; 32], "x", "web", alice())),
        "forbidden"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn expired_and_removed_invitations_refuse() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let p = invite(&gw, spec(false, false), 1);
    let mut old = gw.state.pairing(&p.pid).unwrap().unwrap();
    old.exp = now_s() - 1;
    gw.state.save_pairing(&old).unwrap();
    assert!(!old.admits_handshake(now_s()));
    assert_eq!(
        kind(redeem(&gw, &p.pid, &[1; 32], "x", "web", None)),
        "expired"
    );
    gw.state.remove_pairing(&p.pid).unwrap();
    assert_eq!(
        kind(redeem(&gw, &p.pid, &[1; 32], "x", "web", None)),
        "revoked"
    );
    // A single-use link share: the first opener wins, as before.
    let q = invite(&gw, spec(false, false), 1);
    assert!(redeem(&gw, &q.pid, &[1; 32], "x", "web", None).is_ok());
    assert_eq!(
        kind(redeem(&gw, &q.pid, &[2; 32], "y", "web", None)),
        "used"
    );
    // Peer and handoff invitations keep their own claim.
    let mut sp = spec(false, false);
    sp.kind = "handoff".into();
    let h = invite(&gw, sp, 1);
    assert!(!gw.state.pairing(&h.pid).unwrap().unwrap().redeemable());
    assert_eq!(
        kind(redeem(&gw, &h.pid, &[3; 32], "x", "host", None)),
        "forbidden"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn review_invitations_make_review_devices() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let p = invite(&gw, spec(false, true), 1);
    let link_review = p.share.as_ref().unwrap().review.clone();
    let (d, _) = redeem(&gw, &p.pid, &[1; 32], "Designer", "web", None).unwrap();
    assert_eq!(d.kind, "share");
    assert!(d.is_review());
    assert_eq!(d.review, link_review);
    assert_eq!(d.limit.as_ref().unwrap().preview.as_deref(), Some("pv1"));
    assert!(crate::tui::authorize(&d).is_err());
    // The normal methods' limit matches nothing for a review device.
    let a = crate::api::Allowed::of(&d).unwrap();
    assert!(!a.pane_ok("p1", Some("w1")) && !a.task_ok(Some("w1")));
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_claims_never_pass_the_count() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let p = invite(&gw, spec(true, false), 3);
    let handles: Vec<_> = (0..8u8)
        .map(|i| {
            let (gw, pid) = (gw.clone(), p.pid.clone());
            std::thread::spawn(move || redeem(&gw, &pid, &[10 + i; 32], "x", "web", alice()))
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 3);
    assert!(
        results
            .iter()
            .filter_map(|r| r.as_ref().err())
            .all(|e| e.kind == "used")
    );
    assert_eq!(gw.state.devices().unwrap().len(), 3);
    // One key racing itself makes one device.
    let q = invite(&gw, spec(true, false), 5);
    let handles: Vec<_> = (0..6)
        .map(|_| {
            let (gw, pid) = (gw.clone(), q.pid.clone());
            std::thread::spawn(move || redeem(&gw, &pid, &[99; 32], "x", "web", alice()))
        })
        .collect();
    let ids: Vec<String> = handles
        .into_iter()
        .map(|h| h.join().unwrap().unwrap().0.id)
        .collect();
    assert!(ids.iter().all(|i| *i == ids[0]));
    let stored = gw.state.pairing(&q.pid).unwrap().unwrap();
    assert_eq!(stored.redemptions.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn reconcile_after_a_crash_between_the_writes() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let p = invite(&gw, spec(true, false), 2);
    let (d, _) = redeem(&gw, &p.pid, &[1; 32], "x", "web", alice()).unwrap();
    // The device was written, the pairing write was lost.
    let mut lost = gw.state.pairing(&p.pid).unwrap().unwrap();
    lost.redemptions.clear();
    gw.state.save_pairing(&lost).unwrap();
    {
        let lock = gw.state.lock().unwrap();
        assert_eq!(
            gw.state.reconcile_redemptions(&lock).unwrap(),
            std::slice::from_ref(&p.pid)
        );
        // Nothing left to do the second time.
        assert!(gw.state.reconcile_redemptions(&lock).unwrap().is_empty());
    }
    let fixed = gw.state.pairing(&p.pid).unwrap().unwrap();
    assert_eq!(fixed.redemptions.len(), 1);
    assert_eq!(fixed.redemptions[0].device_id, d.id);
    assert_eq!(fixed.status, PairingStatus::Pending);
    // A retry also heals it without a second device.
    lost.redemptions.clear();
    gw.state.save_pairing(&lost).unwrap();
    let (again, new) = redeem(&gw, &p.pid, &[1; 32], "x", "web", None).unwrap();
    assert_eq!((again.id.as_str(), new), (d.id.as_str(), false));
    // A new gateway reconciles on start.
    lost.redemptions.clear();
    gw.state.save_pairing(&lost).unwrap();
    let st = StateDir::open(t.path().join("gw")).unwrap();
    drop(Gateway::new(st, crate::server::Server::new(t.path().join("none.sock"))).unwrap());
    assert_eq!(
        gw.state.pairing(&p.pid).unwrap().unwrap().redemptions.len(),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoked_openers_stay_spent_and_refused() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let p = invite(&gw, spec(true, false), 2);
    let (a, _) = redeem(&gw, &p.pid, &[1; 32], "x", "web", alice()).unwrap();
    gw.revoke(&a.id).await.unwrap();
    let stored = gw.state.pairing(&p.pid).unwrap().unwrap();
    assert!(stored.redemptions[0].revoked, "a tombstone, not a refund");
    // The revoked key cannot come back, even after a restart.
    assert_eq!(
        kind(redeem(&gw, &p.pid, &[1; 32], "x", "web", alice())),
        "revoked"
    );
    let st = StateDir::open(t.path().join("gw")).unwrap();
    drop(Gateway::new(st, crate::server::Server::new(t.path().join("none.sock"))).unwrap());
    assert_eq!(
        kind(redeem(&gw, &p.pid, &[1; 32], "x", "web", alice())),
        "revoked"
    );
    // Its use stays spent: one use is left, not two.
    assert!(redeem(&gw, &p.pid, &[2; 32], "y", "web", alice()).is_ok());
    assert_eq!(
        kind(redeem(&gw, &p.pid, &[3; 32], "z", "web", alice())),
        "used"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_the_invitation_revokes_its_devices() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let p = invite(&gw, spec(true, false), 3);
    let (a, _) = redeem(&gw, &p.pid, &[1; 32], "x", "web", alice()).unwrap();
    let (b, _) = redeem(&gw, &p.pid, &[2; 32], "y", "web", alice()).unwrap();
    let owner = gw.devices();
    assert_eq!(owner.len(), 2);
    let list = crate::peers::share_list(&gw.state, &gw.devices());
    let inv = &list["invitations"][0];
    assert_eq!(
        (
            inv["access"].as_str(),
            inv["uses"].as_u64(),
            inv["redeemed"].as_u64()
        ),
        (Some("github"), Some(3), Some(2))
    );
    assert_eq!(inv["github"][0]["login"], "alice");
    assert_eq!(list["devices"][0]["invitation"], p.pid.as_str());
    assert_eq!(list["devices"][0]["github"]["id"], 42);
    let r = crate::peers::dispatch(&gw, &a, "share.revoke", &json!({"id": p.pid}))
        .await
        .unwrap();
    assert_eq!(r, json!({"cancelled": "invitation", "devices": 2}));
    assert!(gw.device(&a.id).is_none() && gw.device(&b.id).is_none());
    assert!(gw.state.pairing(&p.pid).unwrap().is_none());
    assert_eq!(
        kind(redeem(&gw, &p.pid, &[3; 32], "z", "web", alice())),
        "revoked"
    );
    // An exhausted invitation is still listed, and can still be revoked as a whole.
    let q = invite(&gw, spec(true, false), 1);
    let (c, _) = redeem(&gw, &q.pid, &[4; 32], "c", "web", alice()).unwrap();
    let list = crate::peers::share_list(&gw.state, &gw.devices());
    assert_eq!(list["invitations"][0]["id"], q.pid.as_str());
    crate::peers::dispatch(&gw, &c, "share.revoke", &json!({"id": q.pid}))
        .await
        .unwrap();
    assert!(gw.device(&c.id).is_none());
}

fn claims(gw: &Gateway, pid: &str, remote: &[u8; 32], gh_id: u64) -> AttestClaims {
    let now = now_s();
    AttestClaims {
        v: 1,
        kid: attest::kid_of(&attest::public_of(&ATTEST_SEED)),
        gh_id,
        login: "alice".into(),
        aud: gw.keys.host_id(),
        nonce: attest::nonce(pid, remote),
        iat: now,
        exp: now + 300,
    }
}

#[test]
fn claims_must_match_host_link_device_and_list() {
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, None);
    let sp = spec(true, false);
    let c = claims(&gw, "pid1", &[1; 32], 42);
    let host = gw.keys.host_id();
    assert_eq!(
        check_claims(&c, &host, "pid1", &[1; 32], &sp).unwrap(),
        GithubId {
            id: 42,
            login: "alice".into()
        }
    );
    assert_eq!(
        kind(check_claims(&c, "another-host", "pid1", &[1; 32], &sp)),
        "attest_invalid"
    );
    assert_eq!(
        kind(check_claims(&c, &host, "pid2", &[1; 32], &sp)),
        "attest_invalid"
    );
    assert_eq!(
        kind(check_claims(&c, &host, "pid1", &[2; 32], &sp)),
        "attest_invalid"
    );
    let other = claims(&gw, "pid1", &[1; 32], 7);
    let e = check_claims(&other, &host, "pid1", &[1; 32], &sp).unwrap_err();
    assert_eq!(e.kind, "github_not_allowed");
    assert_eq!(e.details, json!({"allowed": ["alice"]}));
    // A login is matched by id only: a renamed account keeps its access, a new owner of the
    // old login gets none.
    let renamed = AttestClaims {
        login: "alice-new".into(),
        ..c.clone()
    };
    assert!(check_claims(&renamed, &host, "pid1", &[1; 32], &sp).is_ok());
    let any = ShareSpec {
        github: vec![],
        github_any: true,
        ..sp.clone()
    };
    assert!(check_claims(&other, &host, "pid1", &[1; 32], &any).is_ok());
    let nobody = ShareSpec {
        github: vec![],
        ..sp
    };
    assert_eq!(
        kind(check_claims(&c, &host, "pid1", &[1; 32], &nobody)),
        "github_not_allowed"
    );
}

#[tokio::test]
async fn keys_are_cached_and_refetched_sparingly() {
    use std::sync::atomic::{AtomicU32, Ordering};
    let cache = AttestKeyCache::default();
    let calls = AtomicU32::new(0);
    let key = AttestKey {
        kid: "k".into(),
        key: [1; 32],
    };
    let (calls_ref, key_ref) = (&calls, &key);
    let fetch = move || async move {
        calls_ref.fetch_add(1, Ordering::SeqCst);
        Ok::<_, vk_account::Error>(vec![key_ref.clone()])
    };
    let t0 = Instant::now();
    assert_eq!(cache.keys("s", false, t0, fetch).await.unwrap().len(), 1);
    cache.keys("s", false, t0, fetch).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1, "cached");
    // An unknown kid right after a fetch does not fetch again.
    cache
        .keys("s", true, t0 + Duration::from_secs(30), fetch)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    // A minute later it does, once.
    cache
        .keys("s", true, t0 + REFETCH_MIN, fetch)
        .await
        .unwrap();
    cache
        .keys("s", true, t0 + REFETCH_MIN + Duration::from_secs(1), fetch)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    // After an hour the keys are fetched again; another server is never served from the cache.
    let later = t0 + REFETCH_MIN + KEYS_TTL;
    cache.keys("s", false, later, fetch).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    cache.keys("other", false, later, fetch).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    // A failed refetch keeps the cached keys; a failed first fetch is an error.
    let failing = || async { Err::<Vec<AttestKey>, _>(vk_account::Error::Http("down".into())) };
    assert!(
        cache
            .keys("other", true, t0 + KEYS_TTL + REFETCH_MIN, failing)
            .await
            .is_ok()
    );
    let cold = AttestKeyCache::default();
    assert!(cold.keys("s", false, t0, failing).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn attestations_from_the_account_server() {
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, Some(&fake.url));
    let p = invite(&gw, spec(true, false), 5);
    let sp = p.share.clone().unwrap();
    let remote = [1u8; 32];
    let check = |token: Option<String>| {
        let (gw, p, sp) = (gw.clone(), p.clone(), sp.clone());
        async move { check_attest(&gw, &p, &sp, &remote, token.as_deref()).await }
    };
    // An older app sends none: never an unauthenticated claim.
    assert_eq!(kind(check(None).await), "upgrade_required");
    assert_eq!(kind(check(Some(String::new())).await), "upgrade_required");
    let good = claims(&gw, &p.pid, &remote, 42);
    assert_eq!(
        check(Some(attest::sign(&ATTEST_SEED, &good)))
            .await
            .unwrap()
            .id,
        42
    );
    // Bad signature: another key under the published kid.
    assert_eq!(
        kind(check(Some(attest::sign(&[1; 32], &good))).await),
        "attest_invalid"
    );
    // Expired, issued in the future, for another host, for another device.
    let now = now_s();
    for c in [
        AttestClaims {
            iat: now - 1000,
            exp: now - 700,
            ..good.clone()
        },
        AttestClaims {
            iat: now + 500,
            exp: now + 800,
            ..good.clone()
        },
        AttestClaims {
            aud: "elsewhere".into(),
            ..good.clone()
        },
        AttestClaims {
            nonce: attest::nonce(&p.pid, &[2; 32]),
            ..good.clone()
        },
    ] {
        assert_eq!(
            kind(check(Some(attest::sign(&ATTEST_SEED, &c))).await),
            "attest_invalid",
            "{c:?}"
        );
    }
    // Not on the list.
    let e = check(Some(attest::sign(
        &ATTEST_SEED,
        &claims(&gw, &p.pid, &remote, 7),
    )))
    .await
    .unwrap_err();
    assert_eq!(e.kind, "github_not_allowed");
    // Unknown kid: at most one extra key fetch, then refused.
    let before = fake.with(|s| s.keys_calls);
    let rotated = AttestClaims {
        kid: attest::kid_of(&attest::public_of(&[5; 32])),
        ..good.clone()
    };
    assert_eq!(
        kind(check(Some(attest::sign(&[5; 32], &rotated))).await),
        "attest_invalid"
    );
    assert_eq!(
        kind(check(Some(attest::sign(&[5; 32], &rotated))).await),
        "attest_invalid"
    );
    assert!(fake.with(|s| s.keys_calls) <= before + 1);
}
