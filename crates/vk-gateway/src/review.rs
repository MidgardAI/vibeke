//! Review devices (spec 20 §3.2): a share device with `review: {preview, sid}` reaches one
//! preview and nothing else. It does not get the normal view methods. Its calls pass one
//! function, [`authorize`], before (and instead of) the kind, scope and host-wide checks, which
//! never see them. `authorize` is the complete allow-list, and it rewrites what is forwarded:
//! the target and the author are set here, and fields the device may not choose are dropped.
//!
//! | Method | Rule |
//! |---|---|
//! | `hello`, `ping`, `relay.ticket` | always |
//! | `review.info` | its preview |
//! | `tunnel.*` | in `tunnel.rs`: `tunnel.open {preview}` for its preview only |
//! | `browser.preview_open` | its preview; `confine: true` added, `machine` refused |
//! | `browser.screencast_frame`, `browser.input`, `browser.navigate` (`back`/`forward`/`reload`), `browser.session_close` | sessions it opened |
//! | `browser.annotation_add`, `browser.annotation_pick` | target forced to `{preview}`, `author` injected; picks only in its own session |
//! | `browser.annotations`, `.annotation_update`, `.annotation_remove`, `.annotations_clear`, `.annotations_send` | `author` injected (the server keeps the device to its own drafts); `agent` and `copy_only` refused; `hold: true` when the share holds notes |
//!
//! Everything else is refused, including `pane.*`, `fs.*`, `git.*`, `agent.*`, `tui.*`,
//! `events.subscribe` and `browser.attach_screencast`.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::Gateway;
use crate::api::{ApiError, ApiResult};
use crate::state::{Device, Review};

/// Event types a review device may send to the confined session it opened.
const INPUT_TYPES: &[&str] = &["touch", "tap", "wheel", "text", "key", "history", "resize"];
/// History actions `browser.navigate` accepts from a review device.
const NAV_ACTIONS: &[&str] = &["back", "forward", "reload"];
/// Fields of a pick the device chooses (`browser.annotation_add`). No image: the server renders a
/// crop itself when it can (`capture`).
const ADD_KEYS: &[&str] = &[
    "url", "title", "viewport", "kind", "selector", "role", "name", "text", "html", "rect",
    "covers", "note", "capture",
];

fn forbidden(m: impl Into<String>) -> ApiError {
    ApiError::new("forbidden", m)
}

fn review_of(device: &Device) -> Result<&Review, ApiError> {
    device
        .review
        .as_ref()
        .ok_or_else(|| forbidden("not a review device"))
}

fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}

/// Copy `keys` that are present and not null, plus `op_id`.
fn pick(p: &Value, keys: &[&str]) -> Map<String, Value> {
    let mut m = Map::new();
    for k in keys.iter().chain(&["op_id"]) {
        if let Some(v) = p.get(*k).filter(|v| !v.is_null()) {
            m.insert((*k).to_string(), v.clone());
        }
    }
    m
}

/// Who wrote a review device's notes (spec 20 §3.4): the GitHub account, else the share device.
pub fn author(device: &Device) -> Value {
    match &device.github {
        Some(g) => json!({"kind": "github", "id": g.id.to_string(), "label": g.login}),
        None => json!({"kind": "share", "id": device.id, "label": device.name}),
    }
}

/// A browser session this device opened with `browser.preview_open`; any other is refused.
fn own_session<'a>(gw: &Gateway, device: &Device, p: &'a Value) -> Result<&'a str, ApiError> {
    let session = s(p, "session")
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::invalid("session is required"))?;
    if gw.screencasts.opened_by(session).as_deref() != Some(device.id.as_str()) {
        return Err(forbidden("this review did not open that browser session"));
    }
    Ok(session)
}

fn draft(p: &Value) -> Result<(), ApiError> {
    s(p, "draft")
        .filter(|v| !v.is_empty())
        .map(|_| ())
        .ok_or_else(|| ApiError::invalid("draft is required"))
}

/// Decide a review device's call and return the params to forward (spec 20 §3.2). Never
/// consults the scope tables: a method missing here is refused.
pub fn authorize(gw: &Gateway, device: &Device, method: &str, p: Value) -> Result<Value, ApiError> {
    let review = review_of(device)?;
    if device.expired() {
        return Err(forbidden("this review link expired"));
    }
    if !p.is_object() && !p.is_null() {
        return Err(ApiError::invalid("params must be an object"));
    }
    let preview = review.preview.as_str();
    let mut out = match method {
        "hello" | "ping" | "relay.ticket" | "review.info" => Map::new(),
        "browser.preview_open" => {
            if p.get("machine").is_some_and(|v| !v.is_null()) {
                return Err(forbidden("a review cannot choose the machine"));
            }
            if s(&p, "preview").is_some_and(|v| v != preview)
                || p.get("preview")
                    .is_some_and(|v| !v.is_null() && !v.is_string())
            {
                return Err(forbidden("this review reaches one preview only"));
            }
            let dim = |k: &str| {
                p.get("viewport")
                    .and_then(|v| v.get(k))
                    .and_then(Value::as_u64)
                    .filter(|n| (1..=8192).contains(n))
                    .ok_or_else(|| ApiError::invalid(format!("viewport.{k} must be 1 to 8192")))
            };
            let (width, height) = (dim("width")?, dim("height")?);
            let dpr = p
                .get("dpr")
                .and_then(Value::as_f64)
                .filter(|d| (0.5..=8.0).contains(d))
                .ok_or_else(|| ApiError::invalid("dpr must be 0.5 to 8"))?;
            let mut m = pick(&p, &[]);
            if let Some(dark) = p.get("dark").and_then(Value::as_bool) {
                m.insert("dark".into(), json!(dark));
            }
            m.insert("preview".into(), json!(preview));
            m.insert("viewport".into(), json!({"width": width, "height": height}));
            m.insert("dpr".into(), json!(dpr));
            m.insert(
                "mobile".into(),
                json!(p.get("mobile").and_then(Value::as_bool).unwrap_or(true)),
            );
            m.insert("confine".into(), json!(true));
            m
        }
        "browser.screencast_frame" => {
            own_session(gw, device, &p)?;
            pick(&p, &["session", "after_seq", "wait_ms"])
        }
        "browser.session_close" => {
            own_session(gw, device, &p)?;
            pick(&p, &["session"])
        }
        "browser.input" => {
            own_session(gw, device, &p)?;
            let events = p
                .get("events")
                .and_then(Value::as_array)
                .ok_or_else(|| ApiError::invalid("events is required"))?;
            if events.iter().any(|e| {
                !s(e, "type").is_some_and(|t| INPUT_TYPES.contains(&t))
                    || (s(e, "type") == Some("history")
                        && !s(e, "action").is_some_and(|a| NAV_ACTIONS.contains(&a)))
            }) {
                return Err(forbidden("a review may only tap, scroll, type and go back"));
            }
            pick(&p, &["session", "events"])
        }
        "browser.navigate" => {
            own_session(gw, device, &p)?;
            if ["url", "path", "target", "browser_session"]
                .iter()
                .any(|k| p.get(*k).is_some_and(|v| !v.is_null()))
            {
                return Err(forbidden("a review may only go back, forward or reload"));
            }
            if !s(&p, "action").is_some_and(|a| NAV_ACTIONS.contains(&a)) {
                return Err(ApiError::invalid("action is back, forward or reload"));
            }
            pick(&p, &["session", "action"])
        }
        "browser.annotation_add" => {
            match p.get("target") {
                None | Some(Value::Null) => {}
                Some(t) if t == &json!({"preview": preview}) => {}
                Some(_) => return Err(forbidden("this review annotates one preview only")),
            }
            let mut m = pick(&p, ADD_KEYS);
            m.insert("target".into(), json!({"preview": preview}));
            m
        }
        "browser.annotation_pick" => {
            own_session(gw, device, &p)?;
            pick(&p, &["session", "x", "y", "w", "h"])
        }
        "browser.annotations" => {
            let mut m = pick(&p, &["draft"]);
            m.insert("preview".into(), json!(preview));
            m
        }
        "browser.annotation_update" => {
            draft(&p)?;
            pick(&p, &["draft", "n", "note", "page_note"])
        }
        "browser.annotation_remove" => {
            draft(&p)?;
            pick(&p, &["draft", "n"])
        }
        "browser.annotations_clear" => {
            draft(&p)?;
            pick(&p, &["draft"])
        }
        "browser.annotations_send" => {
            draft(&p)?;
            if p.get("agent").is_some_and(|v| !v.is_null())
                || p.get("copy_only")
                    .is_some_and(|v| v != &json!(false) && !v.is_null())
            {
                return Err(forbidden(
                    "a review sends its notes to the preview's agent only",
                ));
            }
            let mut m = pick(&p, &["draft"]);
            // Anything but an explicit `direct` share holds the notes for the owner.
            if review.notes != "direct" {
                m.insert("hold".into(), json!(true));
            }
            m
        }
        m if m.starts_with("tunnel.") => {
            return Err(forbidden(
                "preview streams are opened on the connection (tunnel.open {preview})",
            ));
        }
        _ => {
            return Err(forbidden(format!(
                "{method} is not available to a design review"
            )));
        }
    };
    if method.starts_with("browser.annotation") {
        out.insert("author".into(), author(device));
    }
    Ok(Value::Object(out))
}

/// Review calls with side effects: they need an `op_id`, like the app API's.
pub fn is_mutating(method: &str) -> bool {
    matches!(
        method,
        "browser.preview_open"
            | "browser.input"
            | "browser.navigate"
            | "browser.session_close"
            | "browser.annotation_add"
            | "browser.annotation_pick"
            | "browser.annotation_update"
            | "browser.annotation_remove"
            | "browser.annotations_clear"
            | "browser.annotations_send"
    )
}

/// `hello` for a review device: who it is and which preview it reaches.
pub fn hello(gw: &Gateway, device: &Device) -> Value {
    let mut r = json!({
        "host_name": gw.host_name,
        "host_id": gw.keys.host_id(),
        "device_id": device.id,
        "scope": device.scope,
        "kind": device.kind,
        "expires_at": device.expires_at,
        "gateway_version": env!("CARGO_PKG_VERSION"),
        "features": ["review"],
    });
    if let Some(rv) = &device.review {
        r["review"] = json!({"preview": rv.preview, "sid": rv.sid});
    }
    if let Some(g) = &device.github {
        r["github"] = json!(g);
    }
    r
}

/// The device is still on the registry and unexpired (it may have been revoked while the call
/// was queued).
fn still_authorized(gw: &Gateway, device: &Device) -> Result<(), ApiError> {
    let _ = gw.reload_devices();
    if gw.device(&device.id).is_none_or(|d| d.expired()) {
        return Err(forbidden("this device is no longer authorized"));
    }
    Ok(())
}

/// Run a call [`authorize`] accepted, with the params it returned.
pub async fn execute(gw: &Arc<Gateway>, device: &Device, method: &str, mut p: Value) -> ApiResult {
    if let Some(m) = p.as_object_mut() {
        m.remove("op_id");
    }
    let actor = format!("gateway:{}", device.name);
    match method {
        "ping" => return Ok(json!({})),
        "hello" => return Ok(hello(gw, device)),
        _ => {}
    }
    still_authorized(gw, device)?;
    match method {
        "relay.ticket" => {
            let (ticket, exp) = gw.device_ticket(device);
            Ok(json!({"ticket": ticket, "exp": exp}))
        }
        "review.info" => info(gw, device).await,
        "browser.preview_open" => {
            // Detached, like the app API's: a cancelled request must not abandon a session the
            // host already opened.
            let (gw, device_id) = (gw.clone(), device.id.clone());
            tokio::spawn(async move {
                crate::screencast::preview_open(&gw, &actor, &device_id, p).await
            })
            .await
            .map_err(|_| ApiError::new("internal", "opening the preview failed"))?
        }
        "browser.screencast_frame" => crate::screencast::frame(gw, &device.id, p).await,
        "browser.session_close" => {
            let session = s(&p, "session").unwrap_or_default().to_string();
            crate::screencast::close_session(gw, &actor, &device.id, &session).await
        }
        "browser.input" | "browser.navigate" => {
            let session = s(&p, "session").unwrap_or_default().to_string();
            if gw.screencasts.controller(&session).as_deref() != Some(device.id.as_str()) {
                return Err(ApiError::new(
                    "conflict",
                    "the review's browser session is not ready",
                ));
            }
            // `browser.navigate` carries only `{session, action}`; the confined session refuses
            // URLs on the server too.
            gw.server.call_as(&actor, method, p).await
        }
        m if m.starts_with("browser.annotation") => gw.server.call_as(&actor, m, p).await,
        _ => Err(forbidden(format!(
            "{method} is not available to a design review"
        ))),
    }
}

/// `review.info {}` → `{preview: {id, handle, label, url, status, port, path}, owner, notes,
/// sid, expires_at}`.
pub async fn info(gw: &Gateway, device: &Device) -> ApiResult {
    let rv = review_of(device)?;
    let r = gw
        .server
        .call("preview.get", json!({"preview": rv.preview}))
        .await;
    let pv = match &r {
        Ok(v) => v.get("preview").cloned().unwrap_or(Value::Null),
        Err(e) if e.kind == "not_found" => Value::Null,
        Err(e) => return Err(e.clone()),
    };
    let field = |k: &str| pv.get(k).cloned().unwrap_or(Value::Null);
    let status = if pv.is_null() {
        json!("gone")
    } else {
        field("status")
    };
    Ok(json!({
        "preview": {
            "id": rv.preview,
            "handle": pv.get("handle").cloned().unwrap_or_else(|| json!(rv.handle)),
            "label": pv.get("label").filter(|v| !v.is_null()).cloned().unwrap_or_else(|| json!(rv.label)),
            "url": field("url"),
            "status": status,
            "port": field("port"),
            "path": field("path"),
        },
        "owner": rv.owner.clone().unwrap_or_else(|| gw.host_name.clone()),
        "notes": rv.notes,
        "sid": rv.sid,
        "expires_at": device.expires_at,
    }))
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod tests;
