//! JSON-RPC 2.0 control API (07 §1–2). One request per line; responses may be out of order.

use crate::core::{Tx, subject_pane};
use crate::{Server, render};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use vk_proto::layout::{self, Direction};
use vk_proto::model::*;
use vk_proto::rpc::{ErrorKind, Request, Response, RpcError};

pub type R = Result<Value, RpcError>;

/// Caller identity for one connection (09 §3.2).
#[derive(Debug, Clone)]
pub struct Ctx {
    pub client_id: String,
    pub kind: String,
    /// Pane whose token authenticated this connection (pane scope), if any.
    pub pane_scope: Option<String>,
    pub remote: bool,
}

pub fn err(kind: ErrorKind, msg: impl Into<String>) -> RpcError {
    RpcError::new(kind, msg)
}

pub fn not_found(what: &str, t: &str) -> RpcError {
    err(ErrorKind::NotFound, format!("{what} not found: {t}")).details(json!({"object": what, "target": t}))
}

pub fn invalid(msg: impl Into<String>) -> RpcError {
    err(ErrorKind::InvalidParams, msg)
}

pub fn internal(e: impl std::fmt::Display) -> RpcError {
    let s = e.to_string();
    if s.contains("storage") || s.contains("database") || s.contains("disk") {
        return err(ErrorKind::StorageUnavailable, s);
    }
    err(ErrorKind::Internal, s)
}

pub fn s<'a>(p: &'a Value, k: &str) -> Option<&'a str> {
    p.get(k).and_then(Value::as_str)
}
pub fn b(p: &Value, k: &str) -> Option<bool> {
    p.get(k).and_then(Value::as_bool)
}
pub fn u(p: &Value, k: &str) -> Option<u64> {
    p.get(k).and_then(Value::as_u64)
}
pub fn req<'a>(p: &'a Value, k: &str) -> Result<&'a str, RpcError> {
    s(p, k).ok_or_else(|| invalid(format!("missing param `{k}`")))
}
pub fn argv(p: &Value, k: &str) -> Option<Vec<String>> {
    match p.get(k) {
        Some(Value::Array(a)) => Some(a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect()),
        Some(Value::String(s)) if !s.is_empty() => Some(vec!["/bin/sh".into(), "-c".into(), s.clone()]),
        _ => None,
    }
}

pub fn cursor(server: &Server, seq: Option<i64>) -> Value {
    server.with_core(|c| {
        let seq = seq.unwrap_or_else(|| c.store.last_seq().unwrap_or(0));
        serde_json::to_value(c.store.cursor(seq)).unwrap_or(Value::Null)
    })
}

/// Resolve a pane target (07 §1.2). With a pane token and no target, `@current`.
pub fn resolve_pane(server: &Server, ctx: &Ctx, target: Option<&str>) -> Result<Pane, RpcError> {
    let t = match target {
        Some(t) if !t.is_empty() => t.to_string(),
        _ => match &ctx.pane_scope {
            Some(p) => p.clone(),
            None => return Err(invalid("pane target required (no pane token for @current)")),
        },
    };
    let id = match t.as_str() {
        "@current" => ctx.pane_scope.clone().ok_or_else(|| invalid("@current needs a pane token (VIBEKE_PANE_TOKEN)"))?,
        "@focused" => server.client_focus(&ctx.client_id).pane.or_else(|| server.focused_pane()).ok_or_else(|| not_found("pane", "@focused"))?,
        other => other.to_string(),
    };
    server.with_core(|c| {
        c.pane(&id).cloned().or_else(|| c.run(&id).and_then(|r| c.pane(&r.pane).cloned())).ok_or_else(|| not_found("pane", &id))
    })
}

pub fn resolve_ws(server: &Server, ctx: &Ctx, target: Option<&str>) -> Result<Workspace, RpcError> {
    let t = match target {
        Some(t) => t.to_string(),
        None => {
            let p = resolve_pane(server, ctx, None).ok();
            match p {
                Some(p) => p.workspace,
                None => server.client_focus(&ctx.client_id).workspace.ok_or_else(|| invalid("workspace target required"))?,
            }
        }
    };
    server.with_core(|c| c.ws(&t).cloned().or_else(|| c.model.workspaces.iter().find(|w| w.display_name() == t).cloned())).ok_or_else(|| not_found("workspace", &t))
}

pub fn resolve_tab(server: &Server, ctx: &Ctx, target: Option<&str>) -> Result<Tab, RpcError> {
    match target {
        Some(t) => server.with_core(|c| c.tab(t).cloned()).ok_or_else(|| not_found("tab", t)),
        None => {
            let p = resolve_pane(server, ctx, None).ok().map(|p| p.tab).or_else(|| server.client_focus(&ctx.client_id).tab);
            let t = p.ok_or_else(|| invalid("tab target required"))?;
            server.with_core(|c| c.tab(&t).cloned()).ok_or_else(|| not_found("tab", &t))
        }
    }
}

/// Handle one JSON-RPC line; returns the response line (without newline). Notifications
/// (no id) still return a response string, which the caller may drop.
pub async fn handle_line(server: &Arc<Server>, ctx: &Ctx, line: &str) -> String {
    let req: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => return serde_json::to_string(&Response::err(Value::Null, err(ErrorKind::ParseError, e.to_string()))).unwrap(),
    };
    let id = req.id.clone().unwrap_or(Value::Null);
    let resp = match dispatch(server, ctx, &req.method, &req.params).await {
        Ok(v) => Response::ok(id, v),
        Err(e) => Response::err(id, e),
    };
    serde_json::to_string(&resp).unwrap()
}

pub const METHODS: &[(&str, bool)] = &[
    ("client.hello", false),
    ("client.list", false),
    ("api.methods", false),
    ("server.status", false),
    ("server.stop", true),
    ("server.reload_config", true),
    ("session.snapshot", false),
    ("workspace.list", false),
    ("workspace.get", false),
    ("workspace.create", true),
    ("workspace.rename", true),
    ("workspace.focus", true),
    ("workspace.close", true),
    ("workspace.move", true),
    ("tab.list", false),
    ("tab.create", true),
    ("tab.rename", true),
    ("tab.focus", true),
    ("tab.close", true),
    ("tab.move", true),
    ("pane.list", false),
    ("pane.get", false),
    ("pane.current", false),
    ("pane.split", true),
    ("pane.focus", true),
    ("pane.close", true),
    ("pane.zoom", true),
    ("pane.resize", true),
    ("pane.equalize", true),
    ("pane.rename", true),
    ("pane.send_text", true),
    ("pane.send_keys", true),
    ("pane.send_bytes", true),
    ("pane.run", true),
    ("pane.read", false),
    ("pane.wait_output", false),
    ("pane.wait_idle", false),
    ("pane.mark_unread", true),
    ("pane.mark_seen", true),
    ("pane.pin", true),
    ("pane.can_see_paths", false),
    ("notification.list", false),
    ("notification.send", true),
    ("notification.read", true),
    ("events.subscribe", false),
    ("events.read", false),
    ("events.wait", false),
    ("search.query", false),
    ("blob.put", true),
    ("image.upload", true),
    ("layout.export", false),
    ("task.create", true),
    ("task.list", false),
    ("task.get", false),
    ("task.finish", true),
    ("worktree.list", false),
    ("worktree.remove", true),
    ("worktree.repo_root", false),
];

async fn dispatch(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> R {
    if let Some(r) = crate::agents::api(server, ctx, method, p).await {
        return r;
    }
    if let Some(r) = crate::run::tasks_api(server, ctx, method, p).await {
        return r;
    }
    match method {
        "client.hello" => Ok(json!({
            "server_version": vk_proto::VERSION,
            "api": vk_proto::API_VERSION,
            "session": server.opts.session,
            "machine": server.opts.machine,
            "capabilities": if ctx.pane_scope.is_some() { json!(["pane"]) } else { json!(["*"]) },
            "features": ["render.v1", "events.v1"],
            "client_id": ctx.client_id,
        })),
        "client.list" => {
            let clients = server.clients.lock().unwrap();
            Ok(json!({"clients": clients.iter().map(|(id, c)| json!({"id": id, "kind": c.kind, "attached_at": c.attached_at_ms, "focused_pane": c.focus.pane})).collect::<Vec<_>>()}))
        }
        "api.methods" => {
            let mut v: Vec<Value> = METHODS.iter().map(|(n, m)| json!({"name": n, "mutating": m})).collect();
            v.extend(crate::agents::METHODS.iter().map(|(n, m)| json!({"name": n, "mutating": m})));
            Ok(json!({"methods": v}))
        }
        "server.status" => {
            let (panes, seq) = server.with_core(|c| (c.model.panes.len(), c.store.last_seq().unwrap_or(0)));
            Ok(json!({
                "pid": std::process::id(),
                "version": vk_proto::VERSION,
                "uptime_ms": server.started.elapsed().as_millis() as u64,
                "session": server.opts.session,
                "machine": server.opts.machine,
                "panes": panes,
                "holders": {"live": server.panes.lock().unwrap().len()},
                "clients": server.clients.lock().unwrap().len(),
                "event_seq": seq,
                "socket": server.paths.socket(),
                "degraded": *server.degraded.lock().unwrap(),
            }))
        }
        "server.stop" => {
            if ctx.pane_scope.is_some() {
                return Err(err(ErrorKind::PermissionDenied, "server.stop is not allowed from a pane token"));
            }
            let kill = b(p, "kill_panes").unwrap_or(false);
            if kill {
                for rt in server.panes.lock().unwrap().values() {
                    rt.send(crate::pane::PaneCmd::Close);
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
            // Snapshot every pane so the next server replays as little as possible.
            for rt in server.panes.lock().unwrap().values() {
                rt.send(crate::pane::PaneCmd::Snapshot);
            }
            let srv = server.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                srv.housekeeping();
                srv.shutdown.notify_waiters();
                let _ = srv.ui.send(crate::UiEvent::Goodbye("server stopped".into()));
                tokio::time::sleep(Duration::from_millis(100)).await;
                std::process::exit(0);
            });
            Ok(json!({}))
        }
        "server.reload_config" => Ok(json!({"changed": [], "errors": []})),
        "session.snapshot" => Ok(server.snapshot_json()),

        // ---- workspaces -----------------------------------------------------------------
        "workspace.list" => {
            let c = server.core.lock().unwrap();
            let list: Vec<Value> = c
                .model
                .workspaces
                .iter()
                .map(|w| {
                    let runs: Vec<&AgentRun> = c.model.runs.iter().filter(|r| c.pane(&r.pane).is_some_and(|p| p.workspace == w.id)).collect();
                    let mut v = serde_json::to_value(w).unwrap();
                    v["name"] = json!(w.display_name());
                    v["tab_count"] = json!(c.tabs_of(&w.id).len());
                    v["pane_count"] = json!(c.model.panes.iter().filter(|p| p.workspace == w.id).count());
                    v["agent_summary"] = json!({
                        "working": runs.iter().filter(|r| r.execution.value == Execution::Working).count(),
                        "idle": runs.iter().filter(|r| r.execution.value == Execution::Idle).count(),
                        "needs_input": c.model.interactions.iter().filter(|i| runs.iter().any(|r| r.id == i.run) && i.status == InteractionStatus::Open).count(),
                    });
                    v
                })
                .collect();
            Ok(json!({"workspaces": list}))
        }
        "workspace.get" => Ok(json!({"workspace": resolve_ws(server, ctx, s(p, "workspace"))?})),
        "workspace.create" => {
            let cwd = s(p, "cwd").map(str::to_string).unwrap_or_else(|| crate::paths::home().to_string_lossy().into_owned());
            let focus = b(p, "focus").unwrap_or(false).then_some(ctx.client_id.as_str());
            let (ws, tab, pane) = server.create_workspace(&cwd, s(p, "name").map(Into::into), argv(p, "command"), focus).map_err(internal)?;
            Ok(json!({"workspace": ws, "tab": tab, "root_pane": pane, "cursor": cursor(server, None)}))
        }
        "workspace.rename" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let mut c = server.core.lock().unwrap();
            let mut w = ws.clone();
            w.name = s(p, "name").map(str::to_string).filter(|s| !s.is_empty());
            let mut tx = Tx::new();
            tx.event("workspace.renamed", json!({"workspace": w.id}), json!({"name": w.name}));
            tx.ws(w.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"workspace": w}))
        }
        "workspace.move" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(0);
            let mut c = server.core.lock().unwrap();
            let mut list = c.model.workspaces.clone();
            let i = list.iter().position(|w| w.id == ws.id).unwrap_or(0) as i64;
            let j = (i + delta).clamp(0, list.len() as i64 - 1) as usize;
            let w = list.remove(i as usize);
            list.insert(j, w);
            let mut tx = Tx::new();
            for (k, mut w) in list.into_iter().enumerate() {
                w.order = k as f64 + 1.0;
                tx.ws(w);
            }
            tx.event("workspace.moved", json!({"workspace": ws.id}), json!({"index": j}));
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({}))
        }
        "workspace.focus" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let pane = server.with_core(|c| {
                let tabs = c.tabs_of(&ws.id);
                let cur = server.client_focus(&ctx.client_id);
                let tab = tabs.iter().find(|t| Some(&t.id) == cur.tab.as_ref()).or(tabs.first()).map(|t| (*t).clone());
                tab.and_then(|t| t.focused_pane.or_else(|| t.layout.panes().first().cloned()))
            });
            if let Some(pane) = pane {
                server.focus_pane(&ctx.client_id, &pane);
            }
            Ok(json!({"workspace": ws}))
        }
        "workspace.close" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let panes: Vec<String> = server.with_core(|c| c.model.panes.iter().filter(|x| x.workspace == ws.id).map(|x| x.id.clone()).collect());
            for pid in panes {
                server.close_pane(&pid);
            }
            Ok(json!({}))
        }

        // ---- tabs -----------------------------------------------------------------------
        "tab.list" => {
            let ws = s(p, "workspace").map(|w| resolve_ws(server, ctx, Some(w))).transpose()?;
            let tabs: Vec<Tab> = server.with_core(|c| c.model.tabs.iter().filter(|t| ws.as_ref().is_none_or(|w| w.id == t.workspace)).cloned().collect());
            Ok(json!({"tabs": tabs}))
        }
        "tab.create" => {
            let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
            let focus = b(p, "focus").unwrap_or(false).then_some(ctx.client_id.as_str());
            let cwd = s(p, "cwd").map(str::to_string).or_else(|| resolve_pane(server, ctx, None).ok().and_then(|x| server.pane_cwd(&x.id)));
            let (tab, pane) = server.create_tab(&ws.id, cwd.as_deref(), s(p, "title").map(Into::into), argv(p, "command"), focus).map_err(internal)?;
            Ok(json!({"tab": tab, "root_pane": pane, "cursor": cursor(server, None)}))
        }
        "tab.rename" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            let mut c = server.core.lock().unwrap();
            let mut t = t.clone();
            t.title = s(p, "title").map(str::to_string).filter(|s| !s.is_empty());
            let mut tx = Tx::new();
            tx.event("tab.renamed", json!({"tab": t.id}), json!({"title": t.title}));
            tx.tab(t.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"tab": t}))
        }
        "tab.focus" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            if let Some(pane) = t.focused_pane.clone().or_else(|| t.layout.panes().first().cloned()) {
                server.focus_pane(&ctx.client_id, &pane);
            }
            Ok(json!({"tab": t}))
        }
        "tab.close" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            for pid in t.layout.panes() {
                server.close_pane(&pid);
            }
            Ok(json!({}))
        }
        "tab.move" => {
            let t = resolve_tab(server, ctx, s(p, "tab"))?;
            let delta = p.get("delta").and_then(Value::as_i64).unwrap_or(0);
            let mut c = server.core.lock().unwrap();
            let mut list: Vec<Tab> = c.tabs_of(&t.workspace).into_iter().cloned().collect();
            let i = list.iter().position(|x| x.id == t.id).unwrap_or(0) as i64;
            let j = (i + delta).clamp(0, list.len() as i64 - 1) as usize;
            let x = list.remove(i as usize);
            list.insert(j, x);
            let mut tx = Tx::new();
            for (k, mut x) in list.into_iter().enumerate() {
                x.order = k as f64 + 1.0;
                tx.tab(x);
            }
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({}))
        }

        // ---- panes ----------------------------------------------------------------------
        "pane.list" => {
            let ws = s(p, "workspace").map(|w| resolve_ws(server, ctx, Some(w))).transpose()?;
            let tab = s(p, "tab").map(|t| resolve_tab(server, ctx, Some(t))).transpose()?;
            let panes: Vec<Value> = server.with_core(|c| {
                c.model
                    .panes
                    .iter()
                    .filter(|x| ws.as_ref().is_none_or(|w| w.id == x.workspace) && tab.as_ref().is_none_or(|t| t.id == x.tab))
                    .filter(|x| !b(p, "has_agent").unwrap_or(false) || c.run_for_pane(&x.id).is_some())
                    .map(|x| {
                        let mut v = serde_json::to_value(x).unwrap();
                        v["agent"] = json!(c.run_for_pane(&x.id).map(|r| json!({"handle": r.handle, "harness": r.harness, "name": r.name, "state": r.execution.value.as_str()})));
                        v
                    })
                    .collect()
            });
            Ok(json!({"panes": panes}))
        }
        "pane.get" | "pane.current" => {
            let pane = resolve_pane(server, ctx, if method == "pane.current" { Some("@current") } else { s(p, "pane") })?;
            let (run, ints) = server.with_core(|c| {
                (c.run_for_pane(&pane.id).cloned(), c.model.interactions.iter().filter(|i| i.pane == pane.id && i.status == InteractionStatus::Open).cloned().collect::<Vec<_>>())
            });
            let rev = server.pane_rt(&pane.id).map(|r| r.rev());
            Ok(json!({"pane": pane, "run": run, "open_interactions": ints, "revision": rev, "cwd": server.pane_cwd(&pane.id)}))
        }
        "pane.split" => {
            let target = resolve_pane(server, ctx, s(p, "pane"))?;
            let dir = Direction::parse(s(p, "direction").unwrap_or("right")).ok_or_else(|| invalid("direction must be right|down|left|up"))?;
            let ratio = p.get("ratio").and_then(Value::as_f64).unwrap_or(0.5) as f32;
            let focus = b(p, "focus").unwrap_or(false).then_some(ctx.client_id.as_str());
            let by = if ctx.pane_scope.is_some() { "agent" } else { "user" };
            let pane = server.split_pane(&target.id, dir, ratio, s(p, "cwd"), argv(p, "command"), s(p, "title").map(Into::into), focus, by).map_err(internal)?;
            Ok(json!({"pane": pane, "cursor": cursor(server, None)}))
        }
        "pane.focus" => {
            let pane = match (s(p, "pane"), s(p, "direction")) {
                (_, Some(d)) => {
                    let cur = resolve_pane(server, ctx, s(p, "pane").or(Some("@focused")))?;
                    let dir = Direction::parse(d).ok_or_else(|| invalid("bad direction"))?;
                    let tab = server.with_core(|c| c.tab(&cur.tab).cloned()).ok_or_else(|| not_found("tab", &cur.tab))?;
                    let rects = layout::rects(&tab.layout, layout::Rect { x: 0, y: 0, w: 400, h: 200 });
                    match layout::neighbor(&rects, &cur.id, dir) {
                        Some(n) => server.with_core(|c| c.pane(&n).cloned()).ok_or_else(|| not_found("pane", &n))?,
                        None => cur,
                    }
                }
                (t, None) => resolve_pane(server, ctx, t)?,
            };
            server.focus_pane(&ctx.client_id, &pane.id);
            Ok(json!({"pane": pane}))
        }
        "pane.close" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            server.close_pane(&pane.id);
            Ok(json!({}))
        }
        "pane.zoom" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let mut c = server.core.lock().unwrap();
            let mut tab = c.tab(&pane.tab).cloned().ok_or_else(|| not_found("tab", &pane.tab))?;
            let on = b(p, "zoomed").unwrap_or(tab.zoomed_pane.as_deref() != Some(&pane.id));
            tab.zoomed_pane = on.then(|| pane.id.clone());
            let mut tx = Tx::new();
            tx.tab(tab.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"tab": tab}))
        }
        "pane.resize" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let dir = Direction::parse(req(p, "direction")?).ok_or_else(|| invalid("bad direction"))?;
            let amount = p.get("percent").and_then(Value::as_f64).unwrap_or(5.0) as f32 / 100.0;
            let mut c = server.core.lock().unwrap();
            let mut tab = c.tab(&pane.tab).cloned().ok_or_else(|| not_found("tab", &pane.tab))?;
            layout::resize(&mut tab.layout, &pane.id, dir, amount);
            let mut tx = Tx::new();
            tx.tab(tab.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"layout": tab.layout}))
        }
        "pane.equalize" => {
            let tab = resolve_tab(server, ctx, s(p, "tab"))?;
            let mut c = server.core.lock().unwrap();
            let mut tab = tab.clone();
            layout::equalize(&mut tab.layout);
            let mut tx = Tx::new();
            tx.tab(tab.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"layout": tab.layout}))
        }
        "pane.rename" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let mut c = server.core.lock().unwrap();
            let mut x = pane.clone();
            x.title = s(p, "title").map(str::to_string).filter(|s| !s.is_empty());
            let mut tx = Tx::new();
            tx.event("pane.title_changed", subject_pane(&x), json!({"title": x.title}));
            tx.pane(x.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"pane": x}))
        }
        "pane.send_text" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let text = req(p, "text")?;
            let modes = render::input_modes(server, &pane.id);
            let bytes = match s(p, "paste").unwrap_or("auto") {
                "raw" => text.as_bytes().to_vec(),
                "bracketed" => {
                    let mut m = modes;
                    m.bracketed_paste = true;
                    vk_term::encode::encode_paste(text, &m)
                }
                _ => {
                    if modes.bracketed_paste {
                        vk_term::encode::encode_paste(text, &modes)
                    } else {
                        text.as_bytes().to_vec()
                    }
                }
            };
            let n = bytes.len();
            send(server, ctx, &pane.id, bytes).await?;
            Ok(json!({"bytes": n}))
        }
        "pane.send_bytes" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let data = req(p, "data_b64")?;
            use base64::Engine;
            let bytes = base64::engine::general_purpose::STANDARD.decode(data).map_err(|e| invalid(e.to_string()))?;
            send(server, ctx, &pane.id, bytes).await?;
            Ok(json!({}))
        }
        "pane.send_keys" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let keys = p.get("keys").and_then(Value::as_array).ok_or_else(|| invalid("keys must be an array"))?;
            let modes = render::input_modes(server, &pane.id);
            // Validate everything before writing a byte (07 §2.6.1).
            let mut bytes = Vec::new();
            for k in keys {
                let k = k.as_str().unwrap_or_default();
                let ev = vk_term::keygrammar::parse_key(k).map_err(|e| err(ErrorKind::InvalidKey, e.to_string()).details(json!({"key": k})))?;
                bytes.extend(vk_term::encode::encode_key(&ev, &modes));
            }
            send(server, ctx, &pane.id, bytes).await?;
            Ok(json!({}))
        }
        "pane.run" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let cmd = req(p, "command")?;
            let rev0 = server.pane_rt(&pane.id).map(|r| r.rev()).unwrap_or(0);
            let mut bytes = cmd.as_bytes().to_vec();
            bytes.push(b'\r');
            send(server, ctx, &pane.id, bytes).await?;
            if b(p, "wait").unwrap_or(false) {
                let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(120_000));
                wait_idle(server, &pane.id, Duration::from_millis(500), timeout, Some(rev0)).await?;
                let text = read_text(server, &pane.id, "recent", 50);
                return Ok(json!({"output_tail": text}));
            }
            Ok(json!({}))
        }
        "pane.read" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let source = s(p, "source").unwrap_or("visible");
            let lines = u(p, "lines").unwrap_or(200) as usize;
            let text = read_text(server, &pane.id, source, lines);
            let rev = server.pane_rt(&pane.id).map(|r| r.rev()).unwrap_or(0);
            Ok(json!({"text": text, "revision": rev, "source": source}))
        }
        "pane.wait_output" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let pat = match (s(p, "regex"), s(p, "match")) {
                (Some(r), _) => regex::Regex::new(r).map_err(|e| invalid(e.to_string()))?,
                (None, Some(m)) => regex::Regex::new(&regex::escape(m)).unwrap(),
                _ => return Err(invalid("match or regex required")),
            };
            let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(30_000));
            let since = u(p, "since_revision");
            let deadline = Instant::now() + timeout;
            let rt = server.pane_rt(&pane.id).ok_or_else(|| not_found("pane", &pane.id))?;
            let mut rx = rt.rev_tx.subscribe();
            loop {
                let rev = rt.rev();
                if since.is_none_or(|s| rev > s) {
                    let text = read_text(server, &pane.id, "recent", 500);
                    if let Some(m) = pat.find(&text) {
                        let line = text[..m.start()].matches('\n').count();
                        return Ok(json!({"matched": m.as_str(), "line": line, "revision": rev}));
                    }
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() || tokio::time::timeout(left, rx.changed()).await.is_err() {
                    return Err(err(ErrorKind::Timeout, "pattern not seen").details(json!({"revision": rt.rev()})));
                }
            }
        }
        "pane.wait_idle" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let quiet = Duration::from_millis(u(p, "quiet_ms").unwrap_or(2000));
            let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(60_000));
            let rev = wait_idle(server, &pane.id, quiet, timeout, None).await?;
            Ok(json!({"revision": rev}))
        }
        "pane.mark_unread" | "pane.mark_seen" | "pane.pin" => {
            let pane = resolve_pane(server, ctx, s(p, "pane"))?;
            let mut c = server.core.lock().unwrap();
            let mut x = pane.clone();
            let mut tx = Tx::new();
            match method {
                "pane.mark_unread" => {
                    x.marked_unread = b(p, "marked").unwrap_or(!x.marked_unread);
                    tx.event("pane.marked_unread", subject_pane(&x), json!({"marked": x.marked_unread}));
                }
                "pane.mark_seen" => {
                    x.unread = false;
                    x.marked_unread = false;
                    if let Some(r) = c.run_for_pane(&x.id) {
                        tx.m.read_mark("local", &x.id, r.done_rev);
                    }
                    tx.event("pane.seen", subject_pane(&x), json!({}));
                }
                _ => {
                    x.pinned = b(p, "pinned").unwrap_or(!x.pinned);
                    tx.event("pane.pinned", subject_pane(&x), json!({"pinned": x.pinned}));
                }
            }
            tx.pane(x.clone());
            server.commit(&mut c, tx).map_err(internal)?;
            Ok(json!({"pane": x}))
        }
        "pane.can_see_paths" => {
            // Host panes on this machine see every path; remote callers ask the remote server,
            // which can't see the client's files (06 A11.1).
            let paths = p.get("paths").and_then(Value::as_array).cloned().unwrap_or_default();
            let visible: Vec<bool> = paths.iter().map(|x| x.as_str().is_some_and(|s| std::path::Path::new(s).exists())).collect();
            Ok(json!({"visible": visible}))
        }

        // ---- notifications ------------------------------------------------------------
        "notification.list" => {
            let unread = b(p, "unread_only").unwrap_or(false);
            let list: Vec<Notification> = server.with_core(|c| c.notifications.iter().rev().filter(|n| !unread || !n.read).take(u(p, "limit").unwrap_or(50) as usize).cloned().collect());
            Ok(json!({"notifications": list}))
        }
        "notification.send" => {
            let pane = resolve_pane(server, ctx, s(p, "pane")).ok().map(|x| x.id);
            let n = server.notify("plugin", pane.as_deref(), req(p, "title")?, s(p, "body").unwrap_or(""), s(p, "urgency").unwrap_or("normal"));
            Ok(json!({"notification": n}))
        }
        "notification.read" => {
            server.with_core(|c| {
                let all = b(p, "all").unwrap_or(false);
                let id = s(p, "notification");
                for n in c.notifications.iter_mut() {
                    if all || Some(n.id.as_str()) == id {
                        n.read = true;
                    }
                }
            });
            Ok(json!({}))
        }

        // ---- events ---------------------------------------------------------------------
        "events.read" => {
            let after = after_seq(server, p)?;
            let types = types(p);
            let limit = u(p, "limit").unwrap_or(500) as usize;
            let events = server.with_core(|c| c.store.events_after(after, limit, &types)).map_err(internal)?;
            let next = events.last().map(|e| e.seq).unwrap_or(after);
            Ok(json!({"events": events, "next": cursor(server, Some(next))}))
        }
        "events.wait" => {
            let after = after_seq(server, p)?;
            let types = types(p);
            let timeout = Duration::from_millis(u(p, "timeout_ms").unwrap_or(60_000));
            let mut rx = server.events.subscribe();
            if let Some(e) = server.with_core(|c| c.store.events_after(after, 1, &types)).map_err(internal)?.into_iter().next() {
                return Ok(json!({"event": e}));
            }
            let deadline = Instant::now() + timeout;
            loop {
                let left = deadline.saturating_duration_since(Instant::now());
                match tokio::time::timeout(left, rx.recv()).await {
                    Ok(Ok(e)) if e.seq > after && (types.is_empty() || types.iter().any(|g| vk_store::glob_match(g, &e.kind))) => return Ok(json!({"event": *e})),
                    Ok(Ok(_)) | Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                    _ => return Err(err(ErrorKind::Timeout, "no matching event")),
                }
            }
        }
        "events.subscribe" => Err(invalid("events.subscribe must be called on a control connection")),

        // ---- search / blobs / layout ----------------------------------------------------
        "search.query" => {
            let q = req(p, "q")?;
            let pane = s(p, "pane").map(|x| resolve_pane(server, ctx, Some(x))).transpose()?.map(|x| x.id);
            let mut hits: Vec<Value> = Vec::new();
            // In-memory scrollback + screen first (newest), then the archive (FTS).
            let panes: Vec<String> = match &pane {
                Some(x) => vec![x.clone()],
                None => server.panes.lock().unwrap().keys().cloned().collect(),
            };
            let needle = q.to_lowercase();
            for pid in &panes {
                let text = read_text(server, pid, "scrollback", 10_000);
                for (i, line) in text.lines().enumerate() {
                    if line.to_lowercase().contains(&needle) {
                        hits.push(json!({"pane": pid, "source": "scrollback", "line": i, "text": line}));
                    }
                }
            }
            let fts = server.with_core(|c| c.store.fts_search(q, pane.as_deref(), u(p, "limit").unwrap_or(50) as usize)).map_err(internal)?;
            for (pid, line, ts, text) in fts {
                hits.push(json!({"pane": pid, "source": "archive", "line": line, "ts": ts, "text": text}));
            }
            hits.truncate(u(p, "limit").unwrap_or(200) as usize);
            Ok(json!({"hits": hits}))
        }
        "blob.put" | "image.upload" => blob_put(server, p),
        "layout.export" => {
            let tab = resolve_tab(server, ctx, s(p, "tab"))?;
            let panes: Vec<Value> = server.with_core(|c| {
                tab.layout.panes().iter().filter_map(|id| c.pane(id)).map(|x| json!({"id": x.id, "cwd": x.cwd, "title": x.title, "command": x.fg_cmdline})).collect()
            });
            Ok(json!({"layout": {"tab": tab, "panes": panes}}))
        }
        _ => Err(err(ErrorKind::MethodNotFound, format!("unknown method {method}"))),
    }
}

fn types(p: &Value) -> Vec<String> {
    match p.get("types") {
        Some(Value::Array(a)) => a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
        _ => vec![],
    }
}

/// `after` may be a full cursor (validated against the log identity) or a bare seq.
pub fn after_seq(server: &Server, p: &Value) -> Result<i64, RpcError> {
    let Some(a) = p.get("after").or_else(|| p.get("cursor")) else { return Ok(p.get("after_seq").and_then(Value::as_i64).unwrap_or(0)) };
    if let Some(n) = a.as_i64() {
        return Ok(n);
    }
    let (sess, epoch, earliest) = server.with_core(|c| (c.store.session_uuid.clone(), c.store.log_epoch.clone(), c.store.earliest_seq().unwrap_or(0)));
    if a.get("session_uuid").and_then(Value::as_str).is_some_and(|x| x != sess) || a.get("log_epoch").and_then(Value::as_str).is_some_and(|x| x != epoch) {
        return Err(err(ErrorKind::Truncated, "cursor_epoch_mismatch").details(json!({"current": cursor(server, None)})));
    }
    let seq = a.get("seq").and_then(Value::as_i64).unwrap_or(0);
    if seq > 0 && seq + 1 < earliest {
        return Err(err(ErrorKind::Truncated, "cursor older than retention").details(json!({"earliest_seq": earliest})));
    }
    Ok(seq)
}

async fn send(server: &Server, ctx: &Ctx, pane: &str, bytes: Vec<u8>) -> Result<(), RpcError> {
    if bytes.is_empty() {
        return Ok(());
    }
    if let Some(reason) = server.agents.input_blocked(pane) {
        return Err(err(ErrorKind::Conflict, reason));
    }
    let _ = ctx;
    let id = server.next_internal_input_id();
    match render::write_and_ack(server, pane, id, bytes).await {
        vk_proto::holder::InputStatus::ChildExited => Err(err(ErrorKind::Conflict, "pane process exited")),
        _ => Ok(()),
    }
}

pub fn read_text(server: &Server, pane: &str, source: &str, lines: usize) -> String {
    let Some(rt) = server.pane_rt(pane) else { return String::new() };
    let sc = rt.screen.lock().unwrap();
    let e = &sc.engine;
    let mut rows: Vec<vk_proto::render::Row> = Vec::new();
    match source {
        "visible" | "detection" => rows = e.visible_rows(),
        _ => {
            let h = e.history_len();
            let want_hist = lines.saturating_sub(e.rows() as usize).min(h);
            for i in h - want_hist..h {
                if let Some(r) = e.history_row(i) {
                    rows.push(r);
                }
            }
            rows.extend(e.visible_rows());
        }
    }
    // Drop trailing blank rows.
    while rows.last().is_some_and(|r| r.text().trim().is_empty()) {
        rows.pop();
    }
    let unwrap = source == "recent_unwrapped" || source == "scrollback";
    let mut out = String::new();
    for (i, r) in rows.iter().enumerate() {
        let t = r.text();
        out.push_str(if r.wrapped && unwrap { &t } else { t.trim_end() });
        if !(r.wrapped && unwrap) && i + 1 < rows.len() {
            out.push('\n');
        }
    }
    let all: Vec<&str> = out.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

pub async fn wait_idle(server: &Server, pane: &str, quiet: Duration, timeout: Duration, changed_since: Option<u64>) -> Result<u64, RpcError> {
    let rt = server.pane_rt(pane).ok_or_else(|| not_found("pane", pane))?;
    let deadline = Instant::now() + timeout;
    let mut rx = rt.rev_tx.subscribe();
    let mut seen_change = changed_since.is_none();
    loop {
        let rev = rt.rev();
        if changed_since.is_some_and(|r| rev > r) {
            seen_change = true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(err(ErrorKind::Timeout, "pane not idle").details(json!({"revision": rev})));
        }
        match tokio::time::timeout(quiet.min(left), rx.changed()).await {
            Err(_) if seen_change => return Ok(rev),
            Err(_) => {}
            Ok(Ok(())) => seen_change = true,
            Ok(Err(_)) => return Err(err(ErrorKind::Conflict, "pane closed")),
        }
    }
}

/// Store an uploaded file (base64) under the pane inbox (06 A11.4) and return its path on this
/// machine. Content-addressed: `<blake3-12>/<basename>`.
pub fn blob_put(server: &Server, p: &Value) -> R {
    use base64::Engine;
    let _ = server;
    let data = match (s(p, "data_b64"), s(p, "path")) {
        (Some(d), _) => base64::engine::general_purpose::STANDARD.decode(d).map_err(|e| invalid(e.to_string()))?,
        (None, Some(path)) => std::fs::read(path).map_err(|e| invalid(format!("read {path}: {e}")))?,
        _ => return Err(invalid("data_b64 or path required")),
    };
    let hash = blake3::hash(&data).to_hex().to_string();
    let name = s(p, "name").map(|n| n.rsplit('/').next().unwrap_or(n).to_string()).filter(|n| !n.is_empty() && n != "." && n != "..").unwrap_or_else(|| {
        let ext = match s(p, "mime").unwrap_or("") {
            "image/png" => "png",
            "image/jpeg" => "jpg",
            "image/gif" => "gif",
            _ => "bin",
        };
        format!("clipboard-{}.{ext}", vk_store::now_ms())
    });
    let dir = crate::paths::Paths::inbox().join(&hash[..12]);
    write_private(&dir, &name, &data).map_err(internal)?;
    let path = dir.join(&name);
    Ok(json!({"hash": hash, "size": data.len(), "path_on_machine": path, "path": path}))
}

fn write_private(dir: &std::path::Path, name: &str, data: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::create_dir_all(dir)?;
    let root = crate::paths::Paths::inbox();
    let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    let path = dir.join(name);
    if path.exists() && std::fs::metadata(&path)?.len() == data.len() as u64 {
        return Ok(()); // same content-addressed file already present
    }
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
    std::io::Write::write_all(&mut f, data)?;
    std::fs::rename(tmp, path)
}
