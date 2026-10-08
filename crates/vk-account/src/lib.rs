//! Relay accounts (spec 16 §6.6): the OAuth device-code login against a hosted relay's control
//! plane, the stored credential (rotating refresh token plus a cached access token), and the
//! short-lived host tokens a gateway presents on `/v1/host?token=`.
//!
//! Headless first: nothing here opens a browser. [`Client::login`] hands the verification URL and
//! user code to a callback and completes by polling alone.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use vk_e2e::b64;
use vk_store::keychain::{Keychain, KeychainError, SERVICE};

/// The hosted relay, and its control plane, when nothing else is configured.
pub const DEFAULT_SERVER: &str = "https://relay.vibeke.dev";
/// `client` sent with `/v1/device/code`.
pub const CLIENT_ID: &str = "vibeke-cli";
/// A cached access token is used only while it has at least this long left.
const ACCESS_MARGIN_S: u64 = 60;
const HTTP_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// No usable credential for this server: run `vibeke login`.
    LoginRequired,
    /// The control plane refused the access token (401).
    Unauthorized,
    /// The device code expired before the login was approved.
    Expired,
    /// The user denied the login.
    Denied,
    /// Transport failure or an unexpected HTTP status.
    Http(String),
    /// The answer did not have the expected shape.
    Protocol(String),
    /// Reading or writing the stored credential failed.
    Store(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::LoginRequired => f.write_str("login required (run: vibeke login)"),
            Error::Unauthorized => f.write_str("the account server refused the access token"),
            Error::Expired => f.write_str("the login code expired; run the login again"),
            Error::Denied => f.write_str("the login was denied"),
            Error::Http(m) => write!(f, "account server: {m}"),
            Error::Protocol(m) => write!(f, "account server: unexpected answer: {m}"),
            Error::Store(m) => write!(f, "account credential: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T, E = Error> = std::result::Result<T, E>;

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `relay.example`, `wss://relay.example/x`, `https://relay.example:443` → canonical
/// `https://relay.example` (the key credentials are stored under).
pub fn server_origin(server: &str) -> Result<String> {
    let s = server.trim();
    let s = if s.contains("://") {
        s.to_string()
    } else {
        format!("https://{s}")
    };
    vk_e2e::relay::canonical_origin(&s).map_err(|e| Error::Protocol(e.to_string()))
}

/// `/v1/device/code` answer.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: Option<String>,
    pub expires_in: u64,
    #[serde(default = "default_interval")]
    pub interval: u64,
}

fn default_interval() -> u64 {
    5
}

/// What the user must do to approve a login.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub verification_uri: String,
    /// The URI with the code filled in (built from `verification_uri` if the server sent none).
    pub verification_uri_complete: String,
    pub user_code: String,
    /// Seconds until the code expires.
    pub expires_in: u64,
}

/// Successful token answer (`/v1/device/token`, `/v1/token/refresh`).
#[derive(Debug, Clone, Deserialize)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: u64,
    pub login: String,
}

/// One device-code poll.
#[derive(Debug, Clone)]
pub enum Poll {
    Pending,
    /// Poll less often: add 5 s to the interval (RFC 8628 §3.5).
    SlowDown,
    Done(Tokens),
}

/// Bytes a host signs with its relay key to claim its host id on the control plane:
/// `"vibeke-cloud/1 host-claim\0" ‖ host_id ‖ "\0" ‖ ts` (unix seconds, decimal). The server accepts
/// `ts` within five minutes of its clock.
pub fn host_claim_message(host_id: &str, ts: u64) -> Vec<u8> {
    [
        HOST_CLAIM.as_bytes(),
        host_id.as_bytes(),
        b"\0",
        ts.to_string().as_bytes(),
    ]
    .concat()
}

const HOST_CLAIM: &str = "vibeke-cloud/1 host-claim\0";

/// A relay host token (`/v1/hosts/{id}/token`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HostToken {
    pub token: String,
    /// Unix seconds.
    pub expires_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Me {
    pub login: String,
    pub account_id: String,
}

/// What a login leaves behind, stored per server.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    /// Canonical origin of the control plane.
    pub server: String,
    pub login: String,
    pub refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_token: Option<String>,
    /// Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access_exp: Option<u64>,
}

impl Credential {
    fn from_tokens(server: &str, t: Tokens) -> Credential {
        Credential {
            server: server.to_string(),
            login: t.login,
            refresh_token: t.refresh_token,
            access_token: Some(t.access_token),
            access_exp: Some(now_s() + t.expires_in),
        }
    }

    /// The cached access token, if it has more than a minute left at `now`.
    pub fn access_at(&self, now: u64) -> Option<&str> {
        match (&self.access_token, self.access_exp) {
            (Some(t), Some(exp)) if exp > now + ACCESS_MARGIN_S => Some(t),
            _ => None,
        }
    }
}

/// The control plane's HTTP API.
#[derive(Clone)]
pub struct Client {
    base: String,
    http: reqwest::Client,
    poll_unit: Duration,
}

impl Client {
    /// A client for the control plane at `server` (any form [`server_origin`] accepts).
    pub fn new(server: &str) -> Result<Client> {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("vibeke/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(Client {
            base: server_origin(server)?,
            http,
            poll_unit: Duration::from_secs(1),
        })
    }

    /// Scale the device-code timing (interval, slow-down step, expiry) for tests.
    pub fn with_poll_unit(mut self, unit: Duration) -> Client {
        self.poll_unit = unit;
        self
    }

    /// Canonical origin of the control plane.
    pub fn server(&self) -> &str {
        &self.base
    }

    async fn send(
        &self,
        method: reqwest::Method,
        path: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> Result<(u16, Value)> {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(t) = bearer {
            req = req.header("authorization", format!("Bearer {t}"));
        }
        if let Some(b) = body {
            req = req
                .header("content-type", "application/json")
                .body(b.to_string());
        }
        let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status().as_u16();
        let bytes = resp.bytes().await.map_err(|e| Error::Http(e.to_string()))?;
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok((status, v))
    }

    async fn post(&self, path: &str, bearer: Option<&str>, body: Value) -> Result<(u16, Value)> {
        self.send(reqwest::Method::POST, path, bearer, Some(body))
            .await
    }

    fn parse<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
        serde_json::from_value(v).map_err(|e| Error::Protocol(e.to_string()))
    }

    fn unexpected(status: u16, v: &Value) -> Error {
        match v.get("error").and_then(|e| e.as_str()) {
            Some(e) => Error::Http(format!("HTTP {status}: {e}")),
            None => Error::Http(format!("HTTP {status}")),
        }
    }

    /// Start a device-code login.
    pub async fn device_code_start(&self, host_id: Option<&str>) -> Result<DeviceCode> {
        let mut body = json!({"client": CLIENT_ID});
        if let Some(h) = host_id {
            body["host_id"] = json!(h);
        }
        match self.post("/v1/device/code", None, body).await? {
            (200, v) => Self::parse(v),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }

    /// Poll once for the login's outcome.
    pub async fn device_code_poll(&self, device_code: &str) -> Result<Poll> {
        let (s, v) = self
            .post(
                "/v1/device/token",
                None,
                json!({"device_code": device_code}),
            )
            .await?;
        if s == 200 {
            return Self::parse(v).map(Poll::Done);
        }
        match v.get("error").and_then(|e| e.as_str()) {
            Some("authorization_pending") => Ok(Poll::Pending),
            Some("slow_down") => Ok(Poll::SlowDown),
            Some("expired_token") => Err(Error::Expired),
            Some("access_denied") => Err(Error::Denied),
            _ => Err(Self::unexpected(s, &v)),
        }
    }

    /// The whole device-code flow: start, show `on_prompt`, poll until approved, denied or
    /// expired. Transient poll failures are retried until the code expires.
    pub async fn login(&self, on_prompt: impl FnMut(&Prompt)) -> Result<Credential> {
        self.login_with(None, on_prompt).await
    }

    /// [`login`](Self::login), telling the server which host is logging in.
    pub async fn login_with(
        &self,
        host_id: Option<&str>,
        mut on_prompt: impl FnMut(&Prompt),
    ) -> Result<Credential> {
        let dc = self.device_code_start(host_id).await?;
        let complete = dc.verification_uri_complete.clone().unwrap_or_else(|| {
            let sep = if dc.verification_uri.contains('?') {
                '&'
            } else {
                '?'
            };
            format!("{}{sep}user_code={}", dc.verification_uri, dc.user_code)
        });
        on_prompt(&Prompt {
            verification_uri: dc.verification_uri.clone(),
            verification_uri_complete: complete,
            user_code: dc.user_code.clone(),
            expires_in: dc.expires_in,
        });
        let deadline = Instant::now() + self.poll_unit * dc.expires_in as u32;
        let mut interval = dc.interval.max(1);
        loop {
            tokio::time::sleep(self.poll_unit * interval as u32).await;
            if Instant::now() > deadline {
                return Err(Error::Expired);
            }
            match self.device_code_poll(&dc.device_code).await {
                Ok(Poll::Pending) => {}
                Ok(Poll::SlowDown) => interval += 5,
                Ok(Poll::Done(t)) => return Ok(Credential::from_tokens(&self.base, t)),
                Err(Error::Http(e)) => tracing::debug!("device token poll: {e}"),
                Err(e) => return Err(e),
            }
        }
    }

    /// Exchange a refresh token (rotating: store the new one). `invalid_grant` →
    /// [`Error::LoginRequired`].
    pub async fn refresh(&self, refresh_token: &str) -> Result<Tokens> {
        match self
            .post(
                "/v1/token/refresh",
                None,
                json!({"refresh_token": refresh_token}),
            )
            .await?
        {
            (200, v) => Self::parse(v),
            (401, _) => Err(Error::LoginRequired),
            (400, v) if s_eq(&v, "error", "invalid_grant") => Err(Error::LoginRequired),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }

    /// A relay token for this host (`/v1/hosts/{id}/token`), about an hour long. The request
    /// carries a [`host_claim_message`] signature by the host's relay key, which proves the caller
    /// owns the host id it claims.
    pub async fn host_token(
        &self,
        access: &str,
        keys: &vk_e2e::HostKeys,
        name: &str,
    ) -> Result<HostToken> {
        let host_id = keys.host_id();
        let ts = now_s();
        let sig = keys.sign(&host_claim_message(&host_id, ts));
        match self
            .post(
                &format!("/v1/hosts/{host_id}/token"),
                Some(access),
                json!({
                    "relay_pub": b64::encode(keys.relay_public()),
                    "name": name,
                    "ts": ts,
                    "sig": b64::encode(sig),
                }),
            )
            .await?
        {
            (200, v) => Self::parse(v),
            (401, _) => Err(Error::Unauthorized),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }

    pub async fn me(&self, access: &str) -> Result<Me> {
        match self
            .send(reqwest::Method::GET, "/v1/me", Some(access), None)
            .await?
        {
            (200, v) => Self::parse(v),
            (401, _) => Err(Error::Unauthorized),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }

    /// Revoke the refresh token (and the session behind it).
    pub async fn logout(&self, refresh_token: &str) -> Result<()> {
        match self
            .post("/v1/logout", None, json!({"refresh_token": refresh_token}))
            .await?
        {
            (200..=299, _) => Ok(()),
            (s, v) => Err(Self::unexpected(s, &v)),
        }
    }
}

fn s_eq(v: &Value, k: &str, want: &str) -> bool {
    v.get(k).and_then(|x| x.as_str()) == Some(want)
}

// ---------------------------------------------------------------------------------------------
// Credential storage

/// Where credentials live, keyed by canonical server origin.
pub trait CredentialStore: Send + Sync {
    fn load(&self, server: &str) -> Result<Option<Credential>>;
    fn save(&self, c: &Credential) -> Result<()>;
    /// `Ok(false)` when there was nothing to delete.
    fn delete(&self, server: &str) -> Result<bool>;
}

/// Keychain account for a server: `account:<host[:port]>` for https, `account:http:<host[:port]>`
/// for plain http (keychain names can't hold `/`).
pub fn keychain_account(server: &str) -> String {
    match server.split_once("://") {
        Some(("https", rest)) => format!("account:{rest}"),
        Some((scheme, rest)) => format!("account:{scheme}:{rest}"),
        None => format!("account:{server}"),
    }
}

/// The OS keychain (service `vibeke`, account [`keychain_account`]), falling back to a 0600 file
/// when the keychain is unavailable or refuses (a headless Linux host without a Secret Service, a
/// background service without keyring access). Reads check both, so the CLI and a gateway
/// service agree even when only one of them can reach the keychain. The value is the credential
/// JSON, base64url-encoded (the macOS backend refuses quotes).
pub struct KeychainStore {
    primary: Option<Keychain>,
    fallback: PathBuf,
}

impl KeychainStore {
    /// OS keychain first, then `fallback` (e.g. `<gateway state dir>/account.json`).
    pub fn new(fallback: PathBuf) -> KeychainStore {
        KeychainStore {
            primary: Some(Keychain::Os),
            fallback,
        }
    }

    /// Only the 0600 file (tests, hosts without a keychain).
    pub fn file_only(path: PathBuf) -> KeychainStore {
        KeychainStore {
            primary: None,
            fallback: path,
        }
    }

    fn file(&self) -> Keychain {
        Keychain::File(self.fallback.clone())
    }

    fn decode(raw: &str) -> Result<Credential> {
        let bytes = b64::decode(raw.trim()).map_err(|_| Error::Store("unreadable".into()))?;
        serde_json::from_slice(&bytes).map_err(|_| Error::Store("unreadable".into()))
    }
}

fn store_err(e: KeychainError) -> Error {
    Error::Store(e.to_string())
}

impl CredentialStore for KeychainStore {
    fn load(&self, server: &str) -> Result<Option<Credential>> {
        let account = keychain_account(server);
        if let Some(k) = &self.primary {
            match k.get(SERVICE, &account) {
                Ok(Some(raw)) => return Self::decode(&raw).map(Some),
                Ok(None) => {}
                Err(e) => tracing::debug!("account credential: {e}; trying the file"),
            }
        }
        match self.file().get(SERVICE, &account).map_err(store_err)? {
            Some(raw) => Self::decode(&raw).map(Some),
            None => Ok(None),
        }
    }

    fn save(&self, c: &Credential) -> Result<()> {
        let account = keychain_account(&c.server);
        let raw = b64::encode(serde_json::to_vec(c).expect("credential serializes"));
        if let Some(k) = &self.primary {
            match k.set(SERVICE, &account, &raw) {
                Ok(()) => {
                    // An older copy in the file must not come back after a keychain delete.
                    let _ = self.file().delete(SERVICE, &account);
                    return Ok(());
                }
                Err(e) => tracing::debug!("account credential: {e}; using the file"),
            }
        }
        self.file().set(SERVICE, &account, &raw).map_err(store_err)
    }

    fn delete(&self, server: &str) -> Result<bool> {
        let account = keychain_account(server);
        let mut gone = false;
        if let Some(k) = &self.primary
            && let Ok(true) = k.delete(SERVICE, &account)
        {
            gone = true;
        }
        gone |= self.file().delete(SERVICE, &account).map_err(store_err)?;
        Ok(gone)
    }
}

/// In-memory store for tests.
#[derive(Default)]
pub struct MemoryStore(Mutex<HashMap<String, Credential>>);

impl CredentialStore for MemoryStore {
    fn load(&self, server: &str) -> Result<Option<Credential>> {
        Ok(self.0.lock().unwrap().get(server).cloned())
    }
    fn save(&self, c: &Credential) -> Result<()> {
        self.0.lock().unwrap().insert(c.server.clone(), c.clone());
        Ok(())
    }
    fn delete(&self, server: &str) -> Result<bool> {
        Ok(self.0.lock().unwrap().remove(server).is_some())
    }
}

// ---------------------------------------------------------------------------------------------
// A logged-in account

/// A [`Client`] plus the stored credential for its server: refreshes and rotates as needed. The
/// credential is re-read from the store on every call, so a `vibeke login` in another process is
/// picked up.
#[derive(Clone)]
pub struct Account {
    client: Client,
    store: Arc<dyn CredentialStore>,
}

impl Account {
    pub fn new(client: Client, store: Arc<dyn CredentialStore>) -> Account {
        Account { client, store }
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn server(&self) -> &str {
        self.client.server()
    }

    /// The stored credential, if any.
    pub fn credential(&self) -> Result<Option<Credential>> {
        self.store.load(self.server())
    }

    /// Store a credential from [`Client::login`].
    pub fn save(&self, c: &Credential) -> Result<()> {
        self.store.save(c)
    }

    /// A valid access token: the cached one, or a refreshed one (`force`: always refresh).
    pub async fn access_token(&self, force: bool) -> Result<String> {
        let cred = self.credential()?.ok_or(Error::LoginRequired)?;
        if !force && let Some(t) = cred.access_at(now_s()) {
            return Ok(t.to_string());
        }
        let t = match self.client.refresh(&cred.refresh_token).await {
            Err(Error::LoginRequired) => {
                // Another process may have rotated the token meanwhile: retry with its token;
                // forget the credential only when it is still the one refused.
                match self.credential()? {
                    Some(now) if now.refresh_token != cred.refresh_token => {
                        self.client.refresh(&now.refresh_token).await?
                    }
                    Some(_) => {
                        let _ = self.store.delete(self.server());
                        return Err(Error::LoginRequired);
                    }
                    None => return Err(Error::LoginRequired),
                }
            }
            r => r?,
        };
        let fresh = Credential::from_tokens(self.server(), t);
        self.store.save(&fresh)?;
        Ok(fresh.access_token.clone().unwrap_or_default())
    }

    /// A relay host token. A refused access token is refreshed once; refused again →
    /// [`Error::LoginRequired`].
    pub async fn host_token(&self, keys: &vk_e2e::HostKeys, name: &str) -> Result<HostToken> {
        let access = self.access_token(false).await?;
        match self.client.host_token(&access, keys, name).await {
            Err(Error::Unauthorized) => {
                let access = self.access_token(true).await?;
                self.client
                    .host_token(&access, keys, name)
                    .await
                    .map_err(|e| match e {
                        Error::Unauthorized => Error::LoginRequired,
                        e => e,
                    })
            }
            r => r,
        }
    }

    pub async fn me(&self) -> Result<Me> {
        let access = self.access_token(false).await?;
        match self.client.me(&access).await {
            Err(Error::Unauthorized) => {
                let access = self.access_token(true).await?;
                self.client.me(&access).await
            }
            r => r,
        }
    }

    /// Revoke the session on the server (best effort) and delete the credential. Returns whether
    /// a credential was stored.
    pub async fn logout(&self) -> Result<bool> {
        let Some(cred) = self.credential()? else {
            return Ok(false);
        };
        if let Err(e) = self.client.logout(&cred.refresh_token).await {
            tracing::warn!("logout: {e}");
        }
        self.store.delete(self.server())
    }
}

#[cfg(any(test, feature = "fake-server"))]
pub mod fake;

#[cfg(test)]
mod tests;
