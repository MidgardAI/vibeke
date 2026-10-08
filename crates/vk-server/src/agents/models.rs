//! Slash-command catalog and structured model control: `agent.commands`, `agent.models`,
//! `agent.set_model`, and the extension control channel (`adapter.control`) they use for
//! interactive pi / omp runs.
//!
//! | Run | `agent.models` / `agent.set_model` |
//! |---|---|
//! | headless Codex (`codex app-server`) | `model/list`; `turn/start {model}` on later turns; `scope: default` also `config/value/write` |
//! | headless pi / omp (`--mode rpc`) | `get_available_models`; `set_model` |
//! | interactive pi / omp with the Vibeke extension | `adapter.control` → `ctx.modelRegistry.getAvailable()`; `pi.setModel()` |
//!
//! pi saves every switch as its default model, so a pi switch needs `scope: default`; the
//! default `scope: session` is refused with `conflict` `reason: "persists_default"` and nothing
//! changes. omp keeps a switch to the session and refuses `scope: default`.
//! | everything else (Claude Code, interactive Codex, screen-only harnesses) | `unsupported`: clients send `/model` and answer the picker |
//!
//! **Control channel.** The extension long-polls `adapter.control {ops}` on its own connection
//! (pane token). A request for that pane is handed to the parked poll as
//! `{request: {id, op, params}}`; the extension carries it out and sends the result with its next
//! poll (`reply: {id, ok, result | error}`). A poll returns `{request: null}` after
//! [`POLL_WAIT`] so the connection never sits idle for long. A pane counts as reachable while a
//! poll is parked or one ended within [`LIVE_FOR`]; otherwise the methods report `unsupported`
//! (an extension too old to poll looks the same).

use super::*;
use std::collections::VecDeque;
use std::sync::LazyLock;

pub(super) async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "agent.commands" => commands(server, ctx, p).await,
        "agent.models" => models(server, ctx, p).await,
        "agent.set_model" => set_model(server, ctx, p).await,
        "adapter.control" => control(ctx, p).await,
        _ => return None,
    })
}

// ---- agent.commands ---------------------------------------------------------------------------

async fn commands(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "target"))?;
    let h = Harness::from_id(&run.harness);
    let mut cmds: Vec<Value> = h
        .and_then(|h| h.manifest())
        .map(|l| {
            l.m.commands
                .iter()
                .map(|c| {
                    json!({"name": c.name, "description": c.description, "takes_arg": c.takes_arg,
                           "opens_picker": c.opens_picker, "dangerous": c.dangerous})
                })
                .collect()
        })
        .unwrap_or_default();
    let mut source = "catalog";
    // Interactive pi / omp: the session's own extension, prompt-template and skill commands.
    if live_pi(&run, h)
        && let Ok(v) = request(
            &run.pane,
            "commands",
            json!({}),
            Duration::from_millis(1500),
        )
        .await
    {
        source = "protocol";
        for c in v
            .get("commands")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(name) = c
                .get("name")
                .and_then(Value::as_str)
                .map(|n| n.trim_start_matches('/'))
                .filter(|n| vk_agents::manifest::valid_command_name(n))
            else {
                continue;
            };
            if cmds.iter().any(|x| x["name"] == name) || cmds.len() >= 500 {
                continue;
            }
            let desc: String = c
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .chars()
                .take(300)
                .collect();
            cmds.push(json!({"name": name, "description": desc, "takes_arg": true,
                             "opens_picker": false, "dangerous": false}));
        }
    }
    Ok(json!({"commands": cmds, "source": source}))
}

// ---- agent.models / agent.set_model -----------------------------------------------------------

/// How a run's model can be controlled.
enum Route {
    /// The headless adapter speaks the protocol.
    Headless(Defaults),
    /// The pi extension's control channel.
    Extension(Defaults),
}

/// How a harness's model switch relates to its saved default model.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Defaults {
    /// A switch stays in the session; `scope: default` saves the default separately (Codex).
    Separate,
    /// Every switch also saves the default: no session-only switch exists (pi).
    Always,
    /// A switch stays in the session; no default can be saved (omp).
    Never,
}

fn unsupported(run: &AgentRun, reason: &str) -> vk_proto::rpc::RpcError {
    err(
        ErrorKind::Unsupported,
        format!(
            "{} offers no structured model control for this run; send /model instead",
            run.harness
        ),
    )
    .details(json!({"reason": reason, "fallback": "/model"}))
}

/// An interactive pi / omp run whose extension polls the control channel.
fn live_pi(run: &AgentRun, h: Option<Harness>) -> bool {
    !headless::is_headless(run) && h.is_some_and(|h| h.is_pi_family()) && reachable(&run.pane)
}

fn route(run: &AgentRun, op: &str) -> Result<Route, vk_proto::rpc::RpcError> {
    if run.ended_at_ms.is_some() {
        return Err(err(ErrorKind::Conflict, "the run has ended"));
    }
    let h = Harness::from_id(&run.harness);
    let omp = h.is_some_and(|h| h.family() == harness::Family::Omp);
    if headless::is_headless(run) {
        return match run.integration.as_str() {
            // Codex saves a default with `config/value/write`; pi's `set_model` always saves
            // one; omp's never does.
            "headless:app-server" => Ok(Route::Headless(Defaults::Separate)),
            "headless:rpc" => Ok(Route::Headless(pi_defaults(omp))),
            _ => Err(unsupported(run, "harness")),
        };
    }
    if h.is_some_and(|h| h.is_pi_family()) {
        if !reachable(&run.pane) || !supports(&run.pane, op) {
            return Err(unsupported(run, "extension_unavailable"));
        }
        return Ok(Route::Extension(pi_defaults(omp)));
    }
    Err(unsupported(run, "harness"))
}

fn pi_defaults(omp: bool) -> Defaults {
    if omp {
        Defaults::Never
    } else {
        Defaults::Always
    }
}

async fn models(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "target"))?;
    let mut v = match route(&run, "models")? {
        Route::Headless(_) => headless::model_op(server, &run, headless::ModelOp::List).await?,
        Route::Extension(_) => {
            request(&run.pane, "models", json!({}), Duration::from_secs(5)).await?
        }
    };
    let models = normalize_models(v.get_mut("models").map(Value::take));
    Ok(json!({"models": models, "source": "protocol"}))
}

/// Keep well-formed entries `{id, label, description?, current}` (at most 500).
fn normalize_models(v: Option<Value>) -> Vec<Value> {
    let Some(Value::Array(a)) = v else {
        return vec![];
    };
    a.into_iter()
        .filter_map(|m| {
            let id = m.get("id").and_then(Value::as_str)?.to_string();
            let label = m
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.is_empty())
                .unwrap_or(&id)
                .to_string();
            let mut o = json!({"id": id, "label": label,
                "current": m.get("current").and_then(Value::as_bool).unwrap_or(false)});
            if let Some(d) = m.get("description").and_then(Value::as_str) {
                o["description"] = json!(d);
            }
            Some(o)
        })
        .take(500)
        .collect()
}

fn valid_model(m: &str) -> bool {
    !m.is_empty() && m.len() <= 200 && m.chars().all(|c| !c.is_whitespace() && !c.is_control())
}

async fn set_model(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "target"))?;
    let model = crate::api::req(p, "model")?.to_string();
    if !valid_model(&model) {
        return Err(invalid(
            "model: a model id without whitespace (at most 200 bytes)",
        ));
    }
    let default = match s(p, "scope").unwrap_or("session") {
        "session" => false,
        "default" => true,
        _ => return Err(invalid("scope: session|default")),
    };
    let route = route(&run, "set_model")?;
    let defaults = match route {
        Route::Headless(d) | Route::Extension(d) => d,
    };
    if default && defaults == Defaults::Never {
        return Err(invalid(format!(
            "scope default: {} has no structured way to save a default model",
            run.harness
        ))
        .details(json!({"reason": "default_unsupported"})));
    }
    // A harness whose every switch saves its default cannot switch for the session only: refuse
    // without touching it, so the client can ask the user and resend with `scope: default`.
    if !default && defaults == Defaults::Always {
        return Err(err(
            ErrorKind::Conflict,
            format!(
                "{} saves every model switch as its default model; resend with scope: default to switch anyway",
                run.harness
            ),
        )
        .details(json!({"reason": "persists_default", "harness": run.harness})));
    }
    let v = match route {
        Route::Headless(_) => {
            headless::model_op(
                server,
                &run,
                headless::ModelOp::Set {
                    model: model.clone(),
                    default,
                },
            )
            .await?
        }
        Route::Extension(_) => {
            request(
                &run.pane,
                "set_model",
                json!({"model": model, "scope": if default { "default" } else { "session" }}),
                Duration::from_secs(10),
            )
            .await?
        }
    };
    let default_changed = v
        .get("default_changed")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let model = v
        .get("model")
        .and_then(Value::as_str)
        .filter(|m| valid_model(m))
        .map(str::to_string)
        .unwrap_or(model);
    update_run(server, &run.id, |r, _| r.model = Some(model.clone()));
    let run = server.with_core(|c| c.run(&run.id).cloned()).unwrap_or(run);
    Ok(json!({"run": run, "default_changed": default_changed}))
}

// ---- control channel --------------------------------------------------------------------------

/// How long a poll stays parked before it returns `{request: null}`.
pub const POLL_WAIT: Duration = Duration::from_secs(25);
/// A pane stays reachable this long after its last poll ended.
const LIVE_FOR: Duration = Duration::from_secs(10);
/// Requests waiting for a poll, per pane.
const QUEUE_MAX: usize = 16;

struct Chan {
    ops: Vec<String>,
    waiter: Option<oneshot::Sender<Value>>,
    queue: VecDeque<Value>,
    last_poll: Instant,
}

#[derive(Default)]
struct Ctl {
    chans: HashMap<String, Chan>,
    /// Request id → (pane, the caller waiting for the reply).
    pending: HashMap<String, (String, oneshot::Sender<Result<Value, String>>)>,
    next: u64,
}

static CTL: LazyLock<Mutex<Ctl>> = LazyLock::new(Default::default);

fn chan_live(c: &Chan) -> bool {
    c.waiter.as_ref().is_some_and(|w| !w.is_closed()) || c.last_poll.elapsed() < LIVE_FOR
}

/// The pane's extension polls the control channel.
pub fn reachable(pane: &str) -> bool {
    CTL.lock().unwrap().chans.get(pane).is_some_and(chan_live)
}

fn supports(pane: &str, op: &str) -> bool {
    CTL.lock()
        .unwrap()
        .chans
        .get(pane)
        .is_some_and(|c| c.ops.iter().any(|o| o == op))
}

/// Send `op` to the pane's extension and wait for its reply.
async fn request(pane: &str, op: &str, params: Value, wait: Duration) -> R {
    let (tx, rx) = oneshot::channel();
    let id = {
        let mut g = CTL.lock().unwrap();
        let ctl = &mut *g;
        let Some(chan) = ctl
            .chans
            .get_mut(pane)
            .filter(|c| chan_live(c) && c.ops.iter().any(|o| o == op))
        else {
            return Err(err(
                ErrorKind::Unsupported,
                format!("the agent's extension does not offer {op}"),
            )
            .details(json!({"reason": "extension_unavailable", "fallback": "/model"})));
        };
        ctl.next += 1;
        let id = format!("c{}", ctl.next);
        let req = json!({"id": id, "op": op, "params": params});
        let undelivered = match chan.waiter.take() {
            Some(w) => w.send(req).err(),
            None => Some(req),
        };
        if let Some(req) = undelivered {
            if chan.queue.len() >= QUEUE_MAX {
                chan.queue.pop_front();
            }
            chan.queue.push_back(req);
        }
        ctl.pending.insert(id.clone(), (pane.to_string(), tx));
        id
    };
    match tokio::time::timeout(wait, rx).await {
        Ok(Ok(Ok(v))) => Ok(v),
        Ok(Ok(Err(e))) => {
            Err(err(ErrorKind::Conflict, e).details(json!({"reason": "harness_error"})))
        }
        _ => {
            let mut g = CTL.lock().unwrap();
            g.pending.remove(&id);
            if let Some(c) = g.chans.get_mut(pane) {
                c.queue.retain(|r| r["id"] != id.as_str());
            }
            Err(err(
                ErrorKind::Timeout,
                format!("the agent did not answer {op}"),
            ))
        }
    }
}

/// `adapter.control {harness?, ops: [string], reply?: {id, ok, result?, error?}, wait_ms?}`.
async fn control(ctx: &Ctx, p: &Value) -> R {
    let Some(pane) = ctx.pane_scope.clone() else {
        return Err(err(
            ErrorKind::PermissionDenied,
            "adapter methods need a pane token",
        ));
    };
    let ops: Vec<String> = p
        .get("ops")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .filter(|o| o.len() <= 32)
                .take(16)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let wait = u(p, "wait_ms")
        .map(Duration::from_millis)
        .unwrap_or(POLL_WAIT)
        .min(POLL_WAIT);
    let rx = {
        let mut g = CTL.lock().unwrap();
        if let Some(r) = p.get("reply")
            && let Some(id) = r.get("id").and_then(Value::as_str)
            && g.pending.get(id).is_some_and(|(owner, _)| *owner == pane)
            && let Some((_, tx)) = g.pending.remove(id)
        {
            let res = if r.get("ok").and_then(Value::as_bool) == Some(true) {
                Ok(r.get("result").cloned().unwrap_or(Value::Null))
            } else {
                Err(r
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("the agent refused the request")
                    .chars()
                    .take(500)
                    .collect())
            };
            let _ = tx.send(res);
        }
        let chan = g.chans.entry(pane.clone()).or_insert_with(|| Chan {
            ops: vec![],
            waiter: None,
            queue: VecDeque::new(),
            last_poll: Instant::now(),
        });
        chan.ops = ops;
        chan.last_poll = Instant::now();
        if let Some(req) = chan.queue.pop_front() {
            return Ok(json!({"request": req}));
        }
        let (tx, rx) = oneshot::channel();
        // A newer poll replaces an older one (its connection is gone or about to be).
        chan.waiter = Some(tx);
        rx
    };
    let r = tokio::time::timeout(wait, rx).await;
    if let Some(c) = CTL.lock().unwrap().chans.get_mut(&pane) {
        c.last_poll = Instant::now();
    }
    Ok(match r {
        Ok(Ok(req)) => json!({"request": req}),
        _ => json!({"request": null}),
    })
}
