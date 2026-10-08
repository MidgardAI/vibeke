//! Requests from the server (`gateway.call`, spec 16 §15.5): the TUI asks its server to run
//! `peer.*` / `share.*` / `devices.*` / `pair.*` here, because the gateway state (keys, devices,
//! peers, invitations) lives in this process. The server emits `gateway.request {id, method,
//! params}` and waits for our `gateway.reply`. Only the methods in [`ALLOWED`] run, through the
//! same functions as the app API where one exists, as the host's owner (`by: "tui"` in the audit
//! log).
//!
//! Two methods exist only here (the TUI's Devices view pairs a phone):
//!
//! - `pair.create {scope?: full|approve|view = full, ttl_s?: int = 600 (60..=3600)}` makes a
//!   confirmed device pairing (the claim is confirmed in the TUI or the terminal, like
//!   `vibeke gateway pair`) => `{link, pid, open_by, scope}`. `unavailable` without a relay or
//!   an app origin.
//! - `pair.status {pid}` => `{status: pending}` | `{status: claimed, name, platform,
//!   fingerprint}` | `{status: done, device_id, name?}` (kept until the sweep removes it, so a retry sees it again) |
//!   `{status: rejected}` | `{status: gone}` (unknown or expired).
//!
//! Cancelling a pairing is `share.revoke {id: pid}` (removes any pairing that isn't done).
//!
//! The TUI also signs the host in to an account relay (spec 16 §6.6) with bridge-only methods
//! (never on the app API, never for a pane):
//!
//! - `account.status {}` => `{needs_account, account_url, logged_in, login, relay}`. The relay's
//!   answer is cached for a minute; an unreachable relay reports the last known answer.
//! - `account.login.start {}` => `{id, verification_uri, verification_uri_complete, user_code,
//!   expires_in}`: a device-code login for this host, or the pending one again. `invalid_params`
//!   with `details.reason: not_needed` when the relay needs no account; `unavailable` when the
//!   account server can't be reached.
//! - `account.login.status {id}` => `{status: pending}` | `{status: done, login}` (the credential
//!   is saved and the relay client retries at once) | `{status: expired}` (also unknown ids) |
//!   `{status: denied}` | `{status: error, message}`.
//! - `account.login.cancel {id}` => `{}`; `account.logout {}` => `{logged_out}`.

use std::sync::Arc;

use serde_json::{Value, json};

use crate::Gateway;
use crate::api::{ApiError, ApiResult};
use crate::state::{Device, PairingStatus, Scope, now_s};

/// Must match the server's allow-list (`vk_server::gateway_bridge::ALLOWED`).
pub const ALLOWED: &[&str] = &[
    "peer.invite",
    "peer.redeem",
    "peer.list",
    "peer.remove",
    "share.create",
    "share.list",
    "share.revoke",
    "devices.list",
    "devices.revoke",
    "pair.create",
    "pair.status",
    "account.status",
    "account.login.start",
    "account.login.status",
    "account.login.cancel",
    "account.logout",
];

/// Who the audit log names for these requests.
pub const BY: &str = "tui";

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_str())
}

/// The host owner as a device: full scope, never stored.
fn owner() -> Device {
    Device {
        id: BY.into(),
        name: "Vibeke TUI".into(),
        platform: "tui".into(),
        public: String::new(),
        scope: Scope::Full,
        paired_at: 0,
        vapid_private: None,
        push: vec![],
        prefs: Default::default(),
        push_failures: 0,
        kind: "device".into(),
        expires_at: None,
        limit: None,
        peer: None,
    }
}

/// Answer server requests until the process stops.
pub async fn run(gw: Arc<Gateway>) {
    let mut rx = gw.hub.subscribe_requests();
    loop {
        match rx.recv().await {
            Ok(req) => {
                tokio::spawn(handle(gw.clone(), req));
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn handle(gw: Arc<Gateway>, req: Value) {
    let Some(id) = s(&req, "id").map(str::to_string) else {
        return;
    };
    let method = s(&req, "method").unwrap_or_default();
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let body = match execute(&gw, method, &params).await {
        Ok(result) => json!({"id": id, "result": result}),
        Err(e) => {
            json!({"id": id, "error": {"kind": e.kind, "message": e.message, "details": e.details}})
        }
    };
    // A refusal means the request already timed out on the server: nothing to do.
    if let Err(e) = gw.server.call("gateway.reply", body).await {
        tracing::debug!("gateway.reply {id}: {}", e.message);
    }
}

/// Run one allowed request (checked again here; the server checked it first).
pub async fn execute(gw: &Arc<Gateway>, method: &str, p: &Value) -> ApiResult {
    if !ALLOWED.contains(&method) || !p.is_object() {
        return Err(ApiError::new(
            "forbidden",
            format!("{method} is not carried by the server bridge"),
        ));
    }
    let owner = owner();
    match method {
        "share.create" => match s(p, "kind") {
            Some("handoff") => crate::api::share_create_as(gw, &owner, p),
            Some("peer") => crate::peers::dispatch(gw, &owner, "peer.invite", p).await,
            _ => Err(ApiError::new(
                "forbidden",
                "the server bridge carries share.create for handoff and peer invitations only",
            )),
        },
        "devices.list" => crate::api::devices_list_as(gw, &owner),
        "devices.revoke" => crate::api::devices_revoke_as(gw, &owner, p).await,
        "pair.create" => pair_create(gw, p),
        "pair.status" => pair_status(gw, p),
        "account.status" => account_status(gw).await,
        "account.login.start" => login_start(gw).await,
        "account.login.status" => login_status(gw, p),
        "account.login.cancel" => login_cancel(gw, p),
        "account.logout" => logout(gw).await,
        m => crate::peers::dispatch(gw, &owner, m, p).await,
    }
}

/// Default and bounds of a TUI pairing link's lifetime (seconds).
const PAIR_TTL_S: u64 = 600;
const PAIR_TTL_MIN_S: u64 = 60;
const PAIR_TTL_MAX_S: u64 = 3600;

/// `pair.create`: a device pairing like `vibeke gateway pair`, confirmed by the operator.
fn pair_create(gw: &Arc<Gateway>, p: &Value) -> ApiResult {
    let scope = match s(p, "scope").unwrap_or("full") {
        "full" => Scope::Full,
        "approve" => Scope::Approve,
        "view" => Scope::View,
        _ => return Err(ApiError::invalid("scope is full, approve or view")),
    };
    let ttl_s = p
        .get("ttl_s")
        .and_then(Value::as_u64)
        .unwrap_or(PAIR_TTL_S)
        .clamp(PAIR_TTL_MIN_S, PAIR_TTL_MAX_S);
    // Both before anything is written: no orphan pairing when the link can't be made.
    let relay = gw.cfg.relay.clone().ok_or_else(|| {
        ApiError::unavailable("no relay configured: run `vibeke gateway pair` once to set one up")
    })?;
    let app = gw.cfg.app_url.clone().ok_or_else(|| {
        ApiError::unavailable(
            "no app origin configured: run `vibeke gateway pair --app-url <origin>` once to set one up",
        )
    })?;
    let (pairing, link) = crate::pair::create_with(
        &gw.state,
        &relay,
        &gw.host_name,
        scope,
        false,
        std::time::Duration::from_secs(ttl_s),
        None,
    )
    .map_err(crate::api::internal)?;
    gw.state.audit(&json!({"ts": now_s(), "event": "pairing.created", "by": BY, "pid": pairing.pid, "scope": scope.as_str()}));
    Ok(json!({
        "link": link.to_url(&app),
        "pid": pairing.pid,
        "open_by": pairing.exp,
        "scope": scope.as_str(),
    }))
}

/// `pair.status`: where a pairing stands. `done` stays readable until the sweep removes the record.
fn pair_status(gw: &Arc<Gateway>, p: &Value) -> ApiResult {
    let pid = s(p, "pid")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::invalid("pid is required"))?;
    let done = {
        let _lock = gw.state.lock().map_err(crate::api::internal)?;
        let pairing = gw
            .state
            .pairing(pid)
            .map_err(|e| ApiError::invalid(e.to_string()))?;
        let Some(pairing) = pairing else {
            return Ok(json!({"status": "gone"}));
        };
        let expired = pairing.exp <= now_s();
        match pairing.status {
            // Kept (the sweep removes it after expiry) so a lost reply can be asked again.
            PairingStatus::Done { device_id } => device_id,
            _ if expired => return Ok(json!({"status": "gone"})),
            PairingStatus::Rejected => return Ok(json!({"status": "rejected"})),
            PairingStatus::Pending => return Ok(json!({"status": "pending"})),
            PairingStatus::Claimed {
                fingerprint,
                device_name,
                platform,
                ..
            } => {
                return Ok(json!({"status": "claimed", "name": device_name,
                                 "platform": platform, "fingerprint": fingerprint}));
            }
        }
    };
    // The claim ran in this process, but the registry may have changed since.
    let _ = gw.reload_devices();
    let mut out = json!({"status": "done", "device_id": done});
    if let Some(d) = gw.device(&done) {
        out["name"] = json!(d.name);
    }
    Ok(out)
}

/// `account.status`: whether the relay needs an account and whether this host has one.
async fn account_status(gw: &Arc<Gateway>) -> ApiResult {
    let need = gw
        .logins
        .status(&gw.cfg, &gw.state.dir, &gw.keys.host_id())
        .await
        .map_err(crate::api::internal)?;
    Ok(json!({
        "needs_account": need.needs_account,
        "account_url": need.account_url,
        "logged_in": need.logged_in,
        "login": need.login,
        "relay": gw.cfg.relay,
    }))
}

/// The account this gateway logs in to.
fn gateway_account(gw: &Gateway) -> Result<vk_account::Account, ApiError> {
    crate::account::account(&crate::account::account_server(&gw.cfg), &gw.state.dir)
        .map_err(|e| ApiError::unavailable(format!("{e:#}")))
}

/// `account.login.start`: a device-code login, or the pending one's prompt again.
async fn login_start(gw: &Arc<Gateway>) -> ApiResult {
    let host = gw.keys.host_id();
    let need = gw
        .logins
        .status(&gw.cfg, &gw.state.dir, &host)
        .await
        .map_err(crate::api::internal)?;
    if !need.needs_account {
        let mut e = ApiError::invalid("this relay doesn't need an account");
        e.details = json!({"reason": "not_needed"});
        return Err(e);
    }
    let acct = gateway_account(gw)?;
    let (id, p) = gw
        .logins
        .start(acct, &host)
        .await
        .map_err(|e| ApiError::unavailable(e.to_string()))?;
    gw.state
        .audit(&json!({"ts": now_s(), "event": "account.login.started", "by": BY}));
    Ok(json!({
        "id": id,
        "verification_uri": p.verification_uri,
        "verification_uri_complete": p.verification_uri_complete,
        "user_code": p.user_code,
        "expires_in": p.expires_in,
    }))
}

fn login_id(p: &Value) -> Result<&str, ApiError> {
    s(p, "id")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::invalid("id is required"))
}

/// `account.login.status`: where a TUI login stands.
fn login_status(gw: &Arc<Gateway>, p: &Value) -> ApiResult {
    use crate::account::LoginStatus;
    Ok(match gw.logins.login_status(login_id(p)?) {
        LoginStatus::Pending => json!({"status": "pending"}),
        LoginStatus::Done(login) => json!({"status": "done", "login": login}),
        LoginStatus::Expired => json!({"status": "expired"}),
        LoginStatus::Denied => json!({"status": "denied"}),
        LoginStatus::Error(m) => json!({"status": "error", "message": m}),
    })
}

/// `account.login.cancel`: abort a pending login.
fn login_cancel(gw: &Arc<Gateway>, p: &Value) -> ApiResult {
    gw.logins.cancel(login_id(p)?);
    Ok(json!({}))
}

/// `account.logout`: revoke the session (best effort) and forget the credential. The relay
/// client reports `login_required` when it next needs a host token.
async fn logout(gw: &Arc<Gateway>) -> ApiResult {
    let acct = gateway_account(gw)?;
    let out = acct
        .logout()
        .await
        .map_err(|e| ApiError::unavailable(e.to_string()))?;
    if out {
        gw.state
            .audit(&json!({"ts": now_s(), "event": "account.logout", "by": BY}));
    }
    Ok(json!({"logged_out": out}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Config, Pairing, StateDir};

    /// A gateway with no server behind it (none of these methods call the server).
    fn gateway(t: &tempfile::TempDir, relay: Option<&str>, app: Option<&str>) -> Arc<Gateway> {
        let st = StateDir::open(t.path().join("gw")).unwrap();
        st.save_config(&Config {
            relay: relay.map(str::to_string),
            app_url: app.map(str::to_string),
            host_name: Some("devbox".into()),
            ..Config::default()
        })
        .unwrap();
        Gateway::new(st, crate::server::Server::new(t.path().join("none.sock"))).unwrap()
    }

    fn device(id: &str, name: &str) -> Device {
        Device {
            id: id.into(),
            name: name.into(),
            platform: "ios".into(),
            public: format!("k-{id}"),
            ..owner()
        }
    }

    /// The server keeps the same list (`vk_server::gateway_bridge::ALLOWED`, tested against this
    /// same literal there).
    #[test]
    fn allowed_matches_the_server() {
        assert_eq!(
            ALLOWED,
            [
                "peer.invite",
                "peer.redeem",
                "peer.list",
                "peer.remove",
                "share.create",
                "share.list",
                "share.revoke",
                "devices.list",
                "devices.revoke",
                "pair.create",
                "pair.status",
                "account.status",
                "account.login.start",
                "account.login.status",
                "account.login.cancel",
                "account.logout",
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unknown_methods_stay_forbidden() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t, None, None);
        for m in ["auth.list", "push.test", "prefs.set", "pair.claim"] {
            assert_eq!(
                execute(&gw, m, &json!({})).await.unwrap_err().kind,
                "forbidden",
                "{m}"
            );
        }
        assert_eq!(
            execute(&gw, "devices.list", &json!([]))
                .await
                .unwrap_err()
                .kind,
            "forbidden"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn devices_list_and_revoke() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t, None, None);
        gw.add_device(device("d1", "phone")).unwrap();
        gw.add_device(device("d2", "tablet")).unwrap();
        let list = execute(&gw, "devices.list", &json!({})).await.unwrap();
        let devs = list["devices"].as_array().unwrap();
        assert_eq!(devs.len(), 2);
        let phone = devs.iter().find(|d| d["id"] == "d1").unwrap();
        assert_eq!(phone["name"], "phone");
        assert_eq!(phone["this"], false, "the TUI is never one of the devices");
        assert!(phone["fingerprint"].is_string());

        assert_eq!(
            execute(&gw, "devices.revoke", &json!({}))
                .await
                .unwrap_err()
                .kind,
            "invalid_params"
        );
        execute(&gw, "devices.revoke", &json!({"device": "d1"}))
            .await
            .unwrap();
        let list = execute(&gw, "devices.list", &json!({})).await.unwrap();
        let ids: Vec<&str> = list["devices"]
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["id"].as_str().unwrap())
            .collect();
        assert_eq!(ids, ["d2"]);
        assert!(gw.state.devices().unwrap().iter().all(|d| d.id != "d1"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pair_create_needs_relay_and_app() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t, None, Some("https://app.example"));
        let e = execute(&gw, "pair.create", &json!({})).await.unwrap_err();
        assert_eq!(e.kind, "unavailable");
        assert!(e.message.contains("no relay"), "{}", e.message);

        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t, Some("wss://relay.example"), None);
        let e = execute(&gw, "pair.create", &json!({})).await.unwrap_err();
        assert_eq!(e.kind, "unavailable");
        assert!(e.message.contains("app origin"), "{}", e.message);
        assert!(
            gw.state.pairings().is_empty(),
            "nothing is written when the link can't be made"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pair_create_status_and_cancel() {
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t, Some("wss://relay.example"), Some("https://app.example"));
        assert_eq!(
            execute(&gw, "pair.create", &json!({"scope": "root"}))
                .await
                .unwrap_err()
                .kind,
            "invalid_params"
        );
        let r = execute(&gw, "pair.create", &json!({"ttl_s": 5}))
            .await
            .unwrap();
        let pid = r["pid"].as_str().unwrap().to_string();
        assert_eq!(r["scope"], "full");
        assert!(
            r["link"]
                .as_str()
                .unwrap()
                .starts_with("https://app.example/#/pair?d=")
        );
        let open_by = r["open_by"].as_u64().unwrap();
        assert!(open_by >= now_s() + 59, "ttl is clamped to at least 60 s");
        let pairing = gw.state.pairing(&pid).unwrap().unwrap();
        assert!(!pairing.no_confirm, "the operator confirms TUI pairings");
        assert!(pairing.share.is_none());
        assert_eq!(pairing.scope, Scope::Full);
        let audit = std::fs::read_to_string(t.path().join("gw/audit.log")).unwrap();
        assert!(audit.contains("pairing.created") && audit.contains("\"by\":\"tui\""));

        let status = |pid: String| {
            let gw = gw.clone();
            async move {
                execute(&gw, "pair.status", &json!({"pid": pid}))
                    .await
                    .unwrap()
            }
        };
        assert_eq!(status(pid.clone()).await, json!({"status": "pending"}));
        assert_eq!(
            execute(&gw, "pair.status", &json!({}))
                .await
                .unwrap_err()
                .kind,
            "invalid_params"
        );
        assert_eq!(
            execute(&gw, "pair.status", &json!({"pid": "../x"}))
                .await
                .unwrap_err()
                .kind,
            "invalid_params"
        );

        // Claimed: the TUI shows who is asking.
        gw.state
            .save_pairing(&Pairing {
                status: PairingStatus::Claimed {
                    claim_id: "c".into(),
                    device_public: "k".into(),
                    fingerprint: "ab-cd".into(),
                    device_name: "phone".into(),
                    platform: "ios".into(),
                },
                ..pairing.clone()
            })
            .unwrap();
        assert_eq!(
            status(pid.clone()).await,
            json!({"status": "claimed", "name": "phone", "platform": "ios", "fingerprint": "ab-cd"})
        );

        // Done: reported with the device's name, again on a retry (a reply may be lost).
        gw.add_device(device("d9", "phone")).unwrap();
        gw.state
            .save_pairing(&Pairing {
                status: PairingStatus::Done {
                    device_id: "d9".into(),
                },
                ..pairing.clone()
            })
            .unwrap();
        assert_eq!(
            status(pid.clone()).await,
            json!({"status": "done", "device_id": "d9", "name": "phone"})
        );
        assert_eq!(status(pid.clone()).await["status"], "done");
        assert_eq!(status("nope".into()).await, json!({"status": "gone"}));

        // Rejected and expired.
        gw.state
            .save_pairing(&Pairing {
                status: PairingStatus::Rejected,
                ..pairing.clone()
            })
            .unwrap();
        assert_eq!(status(pid.clone()).await, json!({"status": "rejected"}));
        gw.state
            .save_pairing(&Pairing {
                exp: now_s() - 1,
                ..pairing.clone()
            })
            .unwrap();
        assert_eq!(status(pid.clone()).await, json!({"status": "gone"}));

        // Cancel: a plain device pairing goes away through share.revoke {id: pid}.
        let r = execute(
            &gw,
            "pair.create",
            &json!({"scope": "view", "ttl_s": 99999}),
        )
        .await
        .unwrap();
        assert_eq!(r["scope"], "view");
        assert!(r["open_by"].as_u64().unwrap() <= now_s() + 3600);
        let pid = r["pid"].as_str().unwrap().to_string();
        assert_eq!(
            execute(&gw, "share.revoke", &json!({"id": pid}))
                .await
                .unwrap(),
            json!({"cancelled": "invitation"})
        );
        assert_eq!(status(pid).await, json!({"status": "gone"}));
    }

    /// A relay stub answering `/v1/status` with `status`; returns its `ws://` URL.
    async fn status_relay(status: Value) -> String {
        let app = axum::Router::new().route(
            "/v1/status",
            axum::routing::get(move || {
                let status = status.clone();
                async move { axum::Json(status) }
            }),
        );
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
        format!("ws://{addr}")
    }

    /// A gateway on `relay` logging in to `account_url`.
    fn account_gateway(t: &tempfile::TempDir, relay: &str, account_url: &str) -> Arc<Gateway> {
        let st = StateDir::open(t.path().join("gw")).unwrap();
        st.save_config(&Config {
            relay: Some(relay.into()),
            account_url: Some(account_url.into()),
            host_name: Some("devbox".into()),
            ..Config::default()
        })
        .unwrap();
        Gateway::new(st, crate::server::Server::new(t.path().join("none.sock"))).unwrap()
    }

    async fn status_of(gw: &Arc<Gateway>, id: &str) -> Value {
        execute(gw, "account.login.status", &json!({"id": id}))
            .await
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn account_on_an_open_relay() {
        let fake = vk_account::fake::FakeServer::start().await;
        let relay = status_relay(json!({"auth": "open"})).await;
        let t = tempfile::tempdir().unwrap();
        let gw = account_gateway(&t, &relay, &fake.url);
        let st = execute(&gw, "account.status", &json!({})).await.unwrap();
        assert_eq!(
            st,
            json!({"needs_account": false, "account_url": null, "logged_in": false,
                   "login": null, "relay": relay})
        );
        let e = execute(&gw, "account.login.start", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.kind, "invalid_params");
        assert_eq!(e.details["reason"], "not_needed");

        // No relay at all: nothing needed either.
        let t = tempfile::tempdir().unwrap();
        let gw = gateway(&t, None, None);
        let st = execute(&gw, "account.status", &json!({})).await.unwrap();
        assert_eq!(st["needs_account"], false);
        assert_eq!(st["relay"], Value::Null);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn account_login_from_the_tui() {
        let fake = vk_account::fake::FakeServer::start().await;
        fake.with(|f| f.pending_polls = 1000);
        let relay = status_relay(json!({"auth": "tickets", "account_url": fake.url.clone()})).await;
        let t = tempfile::tempdir().unwrap();
        let gw = account_gateway(&t, &relay, &fake.url);

        let st = execute(&gw, "account.status", &json!({})).await.unwrap();
        assert_eq!(st["needs_account"], true);
        assert_eq!(st["account_url"], json!(fake.url));
        assert_eq!(
            (&st["logged_in"], &st["login"]),
            (&json!(false), &Value::Null)
        );

        // Start: the prompt; a second start returns the same login.
        let r = execute(&gw, "account.login.start", &json!({}))
            .await
            .unwrap();
        let id = r["id"].as_str().unwrap().to_string();
        assert_eq!(r["user_code"], "WDJB-MJHT");
        assert_eq!(r["expires_in"], 600);
        assert!(
            r["verification_uri"]
                .as_str()
                .unwrap()
                .ends_with("/login/device")
        );
        assert!(
            r["verification_uri_complete"]
                .as_str()
                .unwrap()
                .ends_with("?user_code=WDJB-MJHT")
        );
        let again = execute(&gw, "account.login.start", &json!({}))
            .await
            .unwrap();
        assert_eq!(again, r);
        assert_eq!(status_of(&gw, &id).await, json!({"status": "pending"}));
        assert_eq!(
            status_of(&gw, "nope").await,
            json!({"status": "expired"}),
            "unknown ids read as expired"
        );
        assert_eq!(
            execute(&gw, "account.login.status", &json!({}))
                .await
                .unwrap_err()
                .kind,
            "invalid_params"
        );

        // Approve on the account server: done, and the credential is saved.
        fake.with(|f| f.pending_polls = 0);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let done = loop {
            let s = status_of(&gw, &id).await;
            if s["status"] != "pending" || std::time::Instant::now() > deadline {
                break s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        assert_eq!(done, json!({"status": "done", "login": "octo"}));
        let cred = crate::account::account(&fake.url, &gw.state.dir)
            .unwrap()
            .credential()
            .unwrap()
            .unwrap();
        assert_eq!(cred.login, "octo");
        // The relay client waiting in login_required is woken.
        tokio::time::timeout(std::time::Duration::from_secs(1), gw.logins.done.notified())
            .await
            .expect("login wakes the relay client");
        let st = execute(&gw, "account.status", &json!({})).await.unwrap();
        assert_eq!(
            (&st["logged_in"], &st["login"]),
            (&json!(true), &json!("octo"))
        );
        let audit = std::fs::read_to_string(t.path().join("gw/audit.log")).unwrap();
        assert!(audit.contains("account.login.started"));

        // Logout forgets it.
        assert_eq!(
            execute(&gw, "account.logout", &json!({})).await.unwrap(),
            json!({"logged_out": true})
        );
        assert_eq!(fake.with(|f| f.logouts), 1);
        let st = execute(&gw, "account.status", &json!({})).await.unwrap();
        assert_eq!(
            (&st["logged_in"], &st["login"]),
            (&json!(false), &Value::Null)
        );
        assert_eq!(
            execute(&gw, "account.logout", &json!({})).await.unwrap(),
            json!({"logged_out": false})
        );

        // A new start after a finished one is a new login; cancel drops it.
        fake.with(|f| f.pending_polls = 1000);
        let r = execute(&gw, "account.login.start", &json!({}))
            .await
            .unwrap();
        let id2 = r["id"].as_str().unwrap().to_string();
        assert_ne!(id2, id);
        assert_eq!(status_of(&gw, &id).await, json!({"status": "expired"}));
        assert_eq!(
            execute(&gw, "account.login.cancel", &json!({"id": id2}))
                .await
                .unwrap(),
            json!({})
        );
        assert_eq!(status_of(&gw, &id2).await, json!({"status": "expired"}));
        let r = execute(&gw, "account.login.start", &json!({}))
            .await
            .unwrap();
        assert_ne!(r["id"], json!(id2), "a cancelled login is not resumed");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn account_login_denied_and_unreachable() {
        let fake = vk_account::fake::FakeServer::start().await;
        fake.with(|f| f.deny = true);
        let relay = status_relay(json!({"auth": "tickets", "account_url": fake.url.clone()})).await;
        let t = tempfile::tempdir().unwrap();
        let gw = account_gateway(&t, &relay, &fake.url);
        let r = execute(&gw, "account.login.start", &json!({}))
            .await
            .unwrap();
        let id = r["id"].as_str().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let s = loop {
            let s = status_of(&gw, id).await;
            if s["status"] != "pending" || std::time::Instant::now() > deadline {
                break s;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        };
        assert_eq!(s, json!({"status": "denied"}));

        // An account server nobody answers on: unavailable.
        let t = tempfile::tempdir().unwrap();
        let dead = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            format!("http://{}", l.local_addr().unwrap())
        };
        let gw = account_gateway(&t, &relay, &dead);
        let e = execute(&gw, "account.login.start", &json!({}))
            .await
            .unwrap_err();
        assert_eq!(e.kind, "unavailable", "{}", e.message);
    }
}
