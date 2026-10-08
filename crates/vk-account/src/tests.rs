use std::sync::Arc;
use std::time::Duration;

use super::fake::FakeServer;
use super::*;

const UNIT: Duration = Duration::from_millis(10);

async fn client(f: &FakeServer) -> Client {
    Client::new(&f.url).unwrap().with_poll_unit(UNIT)
}

fn cred(server: &str, refresh: &str, access: Option<(&str, u64)>) -> Credential {
    Credential {
        server: server.into(),
        login: "octo".into(),
        refresh_token: refresh.into(),
        access_token: access.map(|a| a.0.to_string()),
        access_exp: access.map(|a| a.1),
    }
}

#[test]
fn origins_and_keychain_accounts() {
    assert_eq!(
        server_origin("relay.vibeke.dev").unwrap(),
        "https://relay.vibeke.dev"
    );
    assert_eq!(
        server_origin("wss://Relay.Example:443/v1/host").unwrap(),
        "https://relay.example"
    );
    assert_eq!(
        keychain_account("https://relay.vibeke.dev"),
        "account:relay.vibeke.dev"
    );
    assert_eq!(
        keychain_account("http://127.0.0.1:8787"),
        "account:http:127.0.0.1:8787"
    );
}

#[tokio::test]
async fn device_login_happy_path() {
    let f = FakeServer::start().await;
    f.with(|s| s.pending_polls = 2);
    let c = client(&f).await;
    let mut prompts = Vec::new();
    let cred = c.login(|p| prompts.push(p.clone())).await.unwrap();
    assert_eq!(cred.login, "octo");
    assert_eq!(cred.server, f.url);
    assert_eq!(cred.refresh_token, "r1");
    assert_eq!(cred.access_token.as_deref(), Some("a1"));
    assert!(cred.access_at(now_s()).is_some());
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0].user_code, "WDJB-MJHT");
    assert_eq!(
        prompts[0].verification_uri_complete,
        format!("{}/login/device?user_code=WDJB-MJHT", f.url)
    );
    assert_eq!(f.with(|s| s.polls.len()), 3);
}

#[tokio::test]
async fn slow_down_adds_five_intervals() {
    let f = FakeServer::start().await;
    f.with(|s| {
        s.slow_down_at = Some(1);
        s.pending_polls = 1;
    });
    let c = client(&f).await;
    c.login(|_| {}).await.unwrap();
    let polls = f.with(|s| s.polls.clone());
    assert_eq!(polls.len(), 3);
    // Interval 1 unit, then 6 after slow_down.
    assert!(polls[1] - polls[0] >= UNIT * 6, "{:?}", polls[1] - polls[0]);
    assert!(polls[2] - polls[1] >= UNIT * 6);
}

#[tokio::test]
async fn expired_and_denied_codes_end_the_login() {
    let f = FakeServer::start().await;
    f.with(|s| s.expire = true);
    assert_eq!(
        client(&f).await.login(|_| {}).await.unwrap_err(),
        Error::Expired
    );
    f.with(|s| {
        s.expire = false;
        s.deny = true;
    });
    assert_eq!(
        client(&f).await.login(|_| {}).await.unwrap_err(),
        Error::Denied
    );
    // The local deadline also ends it when the server keeps answering pending.
    f.with(|s| {
        s.deny = false;
        s.pending_polls = 1000;
        s.expires_in = 5;
    });
    assert_eq!(
        client(&f).await.login(|_| {}).await.unwrap_err(),
        Error::Expired
    );
}

#[tokio::test]
async fn refresh_rotates_the_stored_token() {
    let f = FakeServer::start().await;
    let c = client(&f).await;
    let first = c.login(|_| {}).await.unwrap();
    let store = Arc::new(MemoryStore::default());
    // Access token about to expire: a refresh is due.
    let mut stale = first.clone();
    stale.access_exp = Some(now_s() + 10);
    store.save(&stale).unwrap();
    let acct = Account::new(c.clone(), store.clone());
    assert_eq!(acct.access_token(false).await.unwrap(), "a2");
    let now = store.load(&f.url).unwrap().unwrap();
    assert_eq!(now.refresh_token, "r2");
    // Cached now.
    assert_eq!(acct.access_token(false).await.unwrap(), "a2");
    assert_eq!(f.with(|s| s.refresh_calls), 1);
    // The old refresh token is spent.
    assert_eq!(c.refresh("r1").await.unwrap_err(), Error::LoginRequired);
}

#[tokio::test]
async fn host_token_refreshes_once_on_401() {
    let f = FakeServer::start().await;
    let c = client(&f).await;
    c.login(|_| {}).await.unwrap();
    let store = Arc::new(MemoryStore::default());
    // A cached access token the server no longer accepts.
    store
        .save(&cred(&f.url, "r1", Some(("revoked", now_s() + 3000))))
        .unwrap();
    let acct = Account::new(c, store.clone());
    let t = acct
        .host_token(&vk_e2e::HostKeys::generate(), "devbox")
        .await
        .unwrap();
    assert_eq!(t.token, "h1");
    assert!(t.expires_at > now_s());
    assert_eq!(f.with(|s| (s.host_token_calls, s.refresh_calls)), (2, 1));
    assert_eq!(
        store.load(&f.url).unwrap().unwrap().access_token.as_deref(),
        Some("a2")
    );
    assert_eq!(acct.me().await.unwrap().login, "octo");
}

#[tokio::test]
async fn invalid_grant_means_login_required_and_forgets_the_credential() {
    let f = FakeServer::start().await;
    let store = Arc::new(MemoryStore::default());
    store.save(&cred(&f.url, "gone", None)).unwrap();
    let acct = Account::new(client(&f).await, store.clone());
    assert_eq!(
        acct.host_token(&vk_e2e::HostKeys::generate(), "devbox")
            .await
            .unwrap_err(),
        Error::LoginRequired
    );
    assert!(store.load(&f.url).unwrap().is_none());
    // Nothing stored at all.
    assert_eq!(
        acct.access_token(false).await.unwrap_err(),
        Error::LoginRequired
    );
}

#[tokio::test]
async fn logout_revokes_and_deletes() {
    let f = FakeServer::start().await;
    let c = client(&f).await;
    let cr = c.login(|_| {}).await.unwrap();
    let store = Arc::new(MemoryStore::default());
    store.save(&cr).unwrap();
    let acct = Account::new(c, store.clone());
    assert!(acct.logout().await.unwrap());
    assert!(store.load(&f.url).unwrap().is_none());
    assert_eq!(f.with(|s| (s.logouts, s.refresh.clone())), (1, None));
    assert!(!acct.logout().await.unwrap());
}

#[test]
fn file_store_roundtrip() {
    let t = tempfile::tempdir().unwrap();
    let path = t.path().join("account.json");
    let store = KeychainStore::file_only(path.clone());
    let c = cred("https://relay.example", "r\"1", Some(("a", 5)));
    assert!(store.load(&c.server).unwrap().is_none());
    store.save(&c).unwrap();
    assert_eq!(store.load(&c.server).unwrap(), Some(c.clone()));
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(store.load("https://other.example").unwrap().is_none());
    assert!(store.delete(&c.server).unwrap());
    assert!(store.load(&c.server).unwrap().is_none());
}

#[test]
fn host_claim_vector() {
    // Shared with the control plane's `tokens::tests::host_claim_vector`.
    let keys = vk_e2e::HostKeys {
        noise_private: [0u8; 32],
        relay_seed: [1u8; 32],
    };
    assert_eq!(
        b64::encode(keys.relay_public()),
        "iojj3XQJ8ZX9UtstPLpdcspnCb8dlBIb83SIAbQPb1w"
    );
    assert_eq!(keys.host_id(), "qnlbvwzzr7mh7dt63azrx7zpzm");
    let msg = host_claim_message(&keys.host_id(), 1_700_000_000);
    assert_eq!(
        b64::encode(&msg),
        "dmliZWtlLWNsb3VkLzEgaG9zdC1jbGFpbQBxbmxidnd6enI3bWg3ZHQ2M2F6cng3enB6bQAxNzAwMDAwMDAw"
    );
    assert_eq!(
        b64::encode(keys.sign(&msg)),
        "i98KX-X7f2HOQWDkpoO-k0G1wRQA32x6RAIrjSpa0TN_HD_aPIzpGVr53IbWl2mW36jXCoKWhu62-o29Y1gJAA"
    );
}
