//! An in-process stand-in for the account control plane (spec 16 §6.6), for tests here and in
//! other crates (feature `fake-server`). Tokens are predictable: the n-th issued pair is
//! `a<n>` / `r<n>`, host tokens are `h<n>`.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde_json::{Value, json};

#[derive(Debug)]
pub struct FakeState {
    /// `authorization_pending` answers before the login is approved.
    pub pending_polls: usize,
    /// Answer this poll (1-based) with `slow_down`.
    pub slow_down_at: Option<usize>,
    pub deny: bool,
    pub expire: bool,
    pub interval: u64,
    pub expires_in: u64,
    /// Lifetime of issued access tokens and host tokens (seconds).
    pub access_ttl: u64,
    pub host_token_ttl: u64,
    /// The currently valid tokens (`None`: none issued yet, or revoked).
    pub access: Option<String>,
    pub refresh: Option<String>,
    pub issued: u32,
    pub polls: Vec<Instant>,
    pub refresh_calls: u32,
    pub host_token_calls: u32,
    pub host_tokens: Vec<String>,
    pub logouts: u32,
}

impl Default for FakeState {
    fn default() -> Self {
        FakeState {
            pending_polls: 0,
            slow_down_at: None,
            deny: false,
            expire: false,
            interval: 1,
            expires_in: 600,
            access_ttl: 3600,
            host_token_ttl: 3600,
            access: None,
            refresh: None,
            issued: 0,
            polls: Vec::new(),
            refresh_calls: 0,
            host_token_calls: 0,
            host_tokens: Vec::new(),
            logouts: 0,
        }
    }
}

impl FakeState {
    fn issue(&mut self) -> Value {
        self.issued += 1;
        let (a, r) = (format!("a{}", self.issued), format!("r{}", self.issued));
        self.access = Some(a.clone());
        self.refresh = Some(r.clone());
        json!({"access_token": a, "refresh_token": r, "expires_in": self.access_ttl, "login": "octo"})
    }
}

pub type Shared = Arc<Mutex<FakeState>>;

/// A running fake server.
pub struct FakeServer {
    /// `http://127.0.0.1:<port>`
    pub url: String,
    pub state: Shared,
}

impl FakeServer {
    pub async fn start() -> FakeServer {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        let state = Shared::default();
        let app = Router::new()
            .route("/v1/device/code", post(code))
            .route("/v1/device/token", post(token))
            .route("/v1/token/refresh", post(refresh))
            .route("/v1/hosts/{id}/token", post(host_token))
            .route("/v1/me", get(me))
            .route("/v1/logout", post(logout))
            .with_state(state.clone());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        FakeServer {
            url: format!("http://{addr}"),
            state,
        }
    }

    pub fn with<T>(&self, f: impl FnOnce(&mut FakeState) -> T) -> T {
        f(&mut self.state.lock().unwrap())
    }
}

fn now_s() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn err(status: StatusCode, e: &str) -> Response {
    (status, axum::Json(json!({"error": e}))).into_response()
}

fn bearer(h: &HeaderMap) -> Option<String> {
    h.get("authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_string)
}

fn body(b: &str) -> Value {
    serde_json::from_str(b).unwrap_or(Value::Null)
}

async fn code(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let st = st.lock().unwrap();
    let host = headers
        .get("host")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost")
        .to_string();
    axum::Json(json!({
        "device_code": "dc1",
        "user_code": "WDJB-MJHT",
        "verification_uri": format!("http://{host}/login/device"),
        "expires_in": st.expires_in,
        "interval": st.interval,
    }))
    .into_response()
}

async fn token(State(st): State<Shared>, b: String) -> Response {
    let mut st = st.lock().unwrap();
    if body(&b)["device_code"] != "dc1" {
        return err(StatusCode::BAD_REQUEST, "invalid_request");
    }
    st.polls.push(Instant::now());
    let n = st.polls.len();
    if st.expire {
        return err(StatusCode::BAD_REQUEST, "expired_token");
    }
    if st.deny {
        return err(StatusCode::BAD_REQUEST, "access_denied");
    }
    if st.slow_down_at == Some(n) {
        return err(StatusCode::BAD_REQUEST, "slow_down");
    }
    let pending = n - usize::from(st.slow_down_at.is_some_and(|s| s < n));
    if pending <= st.pending_polls {
        return err(StatusCode::BAD_REQUEST, "authorization_pending");
    }
    axum::Json(st.issue()).into_response()
}

async fn refresh(State(st): State<Shared>, b: String) -> Response {
    let mut st = st.lock().unwrap();
    st.refresh_calls += 1;
    let given = body(&b)["refresh_token"].as_str().map(str::to_string);
    if given.is_none() || given != st.refresh {
        return err(StatusCode::UNAUTHORIZED, "invalid_grant");
    }
    axum::Json(st.issue()).into_response()
}

async fn host_token(
    State(st): State<Shared>,
    Path(id): Path<String>,
    headers: HeaderMap,
    b: String,
) -> Response {
    let mut st = st.lock().unwrap();
    st.host_token_calls += 1;
    if bearer(&headers).is_none() || bearer(&headers) != st.access {
        return err(StatusCode::UNAUTHORIZED, "invalid_token");
    }
    let b = body(&b);
    let Some(relay_pub) = b["relay_pub"]
        .as_str()
        .and_then(|p| vk_e2e::b64::decode_array::<32>(p).ok())
    else {
        return err(StatusCode::BAD_REQUEST, "invalid_relay_pub");
    };
    if id != vk_e2e::host_id(&relay_pub) {
        return err(StatusCode::BAD_REQUEST, "host_id_mismatch");
    }
    let ts = b["ts"].as_u64().unwrap_or(0);
    if ts.abs_diff(now_s()) > 300 {
        return err(StatusCode::BAD_REQUEST, "stale_timestamp");
    }
    let ok = b["sig"]
        .as_str()
        .and_then(|s| vk_e2e::b64::decode_array::<64>(s).ok())
        .is_some_and(|sig| {
            vk_e2e::keys::verify(&relay_pub, &crate::host_claim_message(&id, ts), &sig)
        });
    if !ok {
        return err(StatusCode::BAD_REQUEST, "invalid_signature");
    }
    let t = format!("h{}", st.host_tokens.len() + 1);
    st.host_tokens.push(t.clone());
    axum::Json(json!({"token": t, "expires_at": now_s() + st.host_token_ttl})).into_response()
}

async fn me(State(st): State<Shared>, headers: HeaderMap) -> Response {
    let st = st.lock().unwrap();
    if bearer(&headers).is_none() || bearer(&headers) != st.access {
        return err(StatusCode::UNAUTHORIZED, "invalid_token");
    }
    axum::Json(json!({"login": "octo", "account_id": "acc1"})).into_response()
}

async fn logout(State(st): State<Shared>, b: String) -> Response {
    let mut st = st.lock().unwrap();
    st.logouts += 1;
    if body(&b)["refresh_token"].as_str().map(str::to_string) == st.refresh {
        st.refresh = None;
        st.access = None;
    }
    StatusCode::NO_CONTENT.into_response()
}
