//! Harness adapters (04): process detection, hook transport, state arbitration (§2.5),
//! interactions with the recoverable delivery transaction (§7.3), gate/observe modes with
//! release-on-focus (§7.2), screen fallback (§9) and best-effort verified keystrokes (§8).

pub mod acp;
pub mod channel;
mod gemini;
#[cfg(test)]
mod golden;
pub mod harness;
pub mod headless;
pub mod hook;
pub mod manifests;
mod opencode;
mod route;
pub mod screen;
mod selfreport;
pub mod usage;

use crate::Server;
use crate::api::{Ctx, R, err, internal, invalid, not_found, resolve_pane, s, u};
use crate::core::{Core, Tx, subject_pane, ulid};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::oneshot;
use vk_proto::holder::ProcStatus;
use vk_proto::model::*;
use vk_proto::rpc::ErrorKind;
use vk_store::now_ms;

pub use harness::{Harness, detect_harness};

pub const METHODS: &[(&str, bool)] = &[
    ("agent.list", false),
    ("agent.get", false),
    ("agent.start", true),
    ("agent.spawn", true),
    ("agent.prompt", true),
    ("agent.wait", false),
    ("agent.interrupt", true),
    ("agent.send_keys", true),
    ("agent.read", false),
    ("agent.rename", true),
    ("agent.release", true),
    ("agent.resume", true),
    ("agent.resumable", false),
    ("agent.harnesses", false),
    ("agent.report", true),
    ("interaction.list", false),
    ("interaction.get", false),
    ("interaction.answer", true),
    ("interaction.cancel", true),
    ("adapter.signal", true),
    ("adapter.gate", true),
    ("adapter.delivery_ack", true),
    // M2 (04 §4.1, §5, §6.6): self-report, manifests.
    ("adapter.report_self", true),
    ("pane.report_agent", true),
    ("pane.report_agent_session", true),
    ("agent.manifests", false),
    ("agent.manifests_reload", true),
];

/// Gate timeout (04 §7.2): after this the hook returns no decision and the native dialog shows.
const GATE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

enum GateReply {
    Decision { json: Value, key: String },
    NoDecision,
}

struct Gate {
    tx: oneshot::Sender<GateReply>,
    pane: String,
}

#[derive(Default)]
struct Inner {
    /// Screen dialogs first seen while a structured transport is healthy: (fingerprint, when).
    /// The structured transport gets a grace period to report the same dialog natively.
    screen_grace: HashMap<String, (String, Instant)>,
    /// harness id → detected version (one `--version` per server lifetime).
    versions: HashMap<String, Option<String>>,
    gates: HashMap<String, Gate>,
    /// Panes whose input is locked while a verified keystroke sequence runs (04 §8).
    locks: HashMap<String, Instant>,
    screen_eval: HashMap<String, Instant>,
    resume_mode: String,
}

const SCREEN_GRACE: Duration = Duration::from_secs(2);

#[derive(Default)]
pub struct Agents {
    inner: Mutex<Inner>,
}

pub fn start(server: &Arc<Server>) {
    manifests::init();
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    {
        let mut i = server.agents.inner.lock().unwrap();
        i.resume_mode = format!("{:?}", cfg.agents.resume_on_restart).to_lowercase();
    }
    // Interactions left `delivering` by a crashed server: the shim reconnect is the reconcile
    // path for Claude/Codex; anything else is unknowable → delivery_unknown (04 §7.3 rule 2).
    // Headless runs are exempt: their journal records whether the response was written, and
    // the pane's adapter settles delivery after its replay (headless::Session::replay_done).
    let stale: Vec<Interaction> = server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| i.delivery == DeliveryState::Delivering)
            .filter(|i| !c.run(&i.run).is_some_and(headless::is_headless))
            .cloned()
            .collect()
    });
    for mut it in stale {
        it.delivery = DeliveryState::DeliveryUnknown;
        it.status = InteractionStatus::Answered;
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.event(
            "interaction.delivery_unknown",
            json!({"interaction": it.id, "pane": it.pane}),
            json!({"reason": "server restarted during delivery"}),
        );
        tx.interaction(it);
        let _ = server.commit(&mut c, tx);
    }
    // Gate waiters died with the old server; their open interactions continue in observe mode.
    let open: Vec<Interaction> = server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| i.status == InteractionStatus::Open && i.gate)
            .cloned()
            .collect()
    });
    for mut it in open {
        it.gate = false;
        server.with_core(|c| {
            let mut tx = Tx::new();
            tx.interaction(it);
            let _ = c.commit(tx);
        });
    }
    // Offer resume for agents that were running before a reboot.
    let resumable = resumable_runs(server);
    if !resumable.is_empty() {
        let auto = server.agents.inner.lock().unwrap().resume_mode == "always";
        if auto {
            let srv = server.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(2)).await;
                for r in resumable {
                    let _ = resume_run(&srv, &r.id, None).await;
                }
            });
        } else {
            server.notify(
                "system",
                None,
                &format!("{} agent(s) can be resumed", resumable.len()),
                "vibeke agent resumable · vibeke agent resume <run>",
                "normal",
            );
        }
    }
}

/// Write PATH shims (04 §6.2): `codex` → adds `--disable daemon_auto_start` so the TUI runs a
/// per-pane embedded app-server whose hooks carry this pane's identity.
pub fn install_shims(bin: &std::path::Path) -> std::io::Result<()> {
    let dir = crate::paths::Paths::shims();
    std::fs::create_dir_all(&dir)?;
    let script = harness::codex_shim_script();
    let path = dir.join("codex");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(script.as_str()) {
        std::fs::write(&path, &script)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    }
    let _ = bin;
    Ok(())
}

fn source_of(s: &str) -> StateSource {
    match s {
        "screen" => StateSource::Screen,
        "process" => StateSource::Process,
        "self_report" => StateSource::SelfReport,
        _ => StateSource::Structured,
    }
}

fn facet(value: Execution, source: StateSource, confidence: f32) -> Facet<Execution> {
    Facet {
        value,
        since_ms: now_ms(),
        source,
        confidence,
        detail: None,
    }
}

pub fn resumable_runs(server: &Server) -> Vec<AgentRun> {
    let ended: Vec<AgentRun> =
        server.with_core(|c| c.store.load_closed("run", 50).unwrap_or_default());
    let live: Vec<String> = server.with_core(|c| {
        c.model
            .runs
            .iter()
            .filter_map(|r| r.harness_session_id.clone())
            .collect()
    });
    let mut seen = std::collections::HashSet::new();
    ended
        .into_iter()
        .filter(|r: &AgentRun| r.ended_at_ms.is_some_and(|t| now_ms() - t < 7 * 86_400_000))
        .filter(|r| matches!(r.execution.detail.as_deref(), Some("holder_lost")))
        .filter(|r| {
            !r.resume_argv.is_empty()
                && r.harness_session_id
                    .as_ref()
                    .is_none_or(|s| !live.contains(s))
        })
        .filter(|r| seen.insert(r.harness_session_id.clone().unwrap_or_else(|| r.id.clone())))
        .collect()
}

impl Agents {
    /// Reject client input while a verified keystroke sequence owns the pane (04 §8).
    /// Hold the pane's input lock (other clients' and API input is refused) while Vibeke itself
    /// delivers input (keystroke answers, task messages).
    pub fn lock_input(&self, pane: &str) {
        self.inner
            .lock()
            .unwrap()
            .locks
            .insert(pane.to_string(), Instant::now());
    }

    pub fn unlock_input(&self, pane: &str) {
        self.inner.lock().unwrap().locks.remove(pane);
    }

    pub fn input_blocked(&self, pane: &str) -> Option<&'static str> {
        let i = self.inner.lock().unwrap();
        match i.locks.get(pane) {
            Some(t) if t.elapsed() < Duration::from_secs(2) => {
                Some("input_locked_open_interaction")
            }
            _ => None,
        }
    }

    /// Release-on-focus (04 §7.2): a held gate for this pane returns "no decision" so the
    /// harness's own dialog appears in the focused pane.
    pub fn on_focus(&self, server: &Server, pane: &str) {
        let released: Vec<(String, Gate)> = {
            let mut i = self.inner.lock().unwrap();
            let ids: Vec<String> = i
                .gates
                .iter()
                .filter(|(_, g)| g.pane == pane)
                .map(|(k, _)| k.clone())
                .collect();
            ids.into_iter()
                .filter_map(|k| i.gates.remove(&k).map(|g| (k, g)))
                .collect()
        };
        for (id, g) in released {
            let _ = g.tx.send(GateReply::NoDecision);
            let mut c = server.core.lock().unwrap();
            if let Some(mut it) = c.interaction(&id).cloned() {
                it.gate = false;
                let mut tx = Tx::new();
                tx.event(
                    "interaction.updated",
                    json!({"interaction": it.id, "pane": it.pane}),
                    json!({"gate": false, "reason": "released_on_focus"}),
                );
                tx.interaction(it);
                let _ = server.commit(&mut c, tx);
            }
        }
    }

    /// Process detection (04 §5.2): foreground process tree → harness.
    pub fn on_process(&self, server: &Arc<Server>, pane: &str, st: &ProcStatus) {
        // A pipe pane's process is its headless harness: the adapter owns the run.
        if server.pane_rt(pane).is_some_and(|rt| rt.is_pipe()) {
            return;
        }
        let detected = st
            .fg_pgid
            .and_then(|pg| manifests::detect_pane(server, pane, pg));
        let current = server.with_core(|c| c.run_for_pane(pane).cloned());
        match (detected, current) {
            (Some((h, argv)), None) => {
                let yolo = harness::yolo(h, &argv);
                let mut c = server.core.lock().unwrap();
                let mut tx = Tx::new();
                let run = new_run(&mut c, pane, h, "process", StateSource::Process, 0.6);
                let mut run = run;
                run.yolo = yolo;
                run.resume_argv = vec![];
                tx.event(
                    "agent.detected",
                    json!({"run": run.id, "pane": pane}),
                    json!({"harness": h.id(), "via": "process", "argv0": argv.first()}),
                );
                tx.counters = true;
                let run_id = run.id.clone();
                tx.run(run);
                let _ = server.commit(&mut c, tx);
                drop(c);
                check_version(server, &run_id, h);
            }
            (Some((h, argv)), Some(r)) if r.harness != h.id() => {
                self.end_run(server, &r.id, "replaced");
                let _ = argv;
                self.on_process(server, pane, st);
            }
            (None, Some(r)) => {
                // The run ended only when the foreground is back at the pane's own shell (a
                // tool the agent runs in the foreground must not end it).
                if st.fg_pgid == Some(st.child_pid) {
                    self.end_run(server, &r.id, "exited");
                }
            }
            (Some((h, argv)), Some(r)) => {
                let yolo = harness::yolo(h, &argv)
                    || r.permission_mode
                        .as_deref()
                        .is_some_and(|m| m == "bypassPermissions");
                if yolo != r.yolo {
                    update_run(server, &r.id, |r, _| r.yolo = yolo);
                }
            }
            (None, None) => {}
        }
    }

    pub fn end_run(&self, server: &Server, run: &str, reason: &str) {
        let mut c = server.core.lock().unwrap();
        let Some(r) = c.run(run).cloned() else { return };
        let mut tx = Tx::new();
        self.end_run_tx(&mut c, &mut tx, &r, reason);
        let _ = server.commit(&mut c, tx);
    }

    /// Process death overrides immediately and cancels open interactions (04 §2.5 rule 1).
    pub fn end_run_tx(&self, core: &mut Core, tx: &mut Tx, run: &AgentRun, reason: &str) {
        let mut r = run.clone();
        r.ended_at_ms = Some(now_ms());
        crate::items::end_run_tx(core, tx, &r.id);
        let prev = r.execution.value.clone();
        r.execution = Facet {
            detail: Some(reason.to_string()),
            ..facet(Execution::Exited, StateSource::Process, 1.0)
        };
        tx.event("agent.state_changed", json!({"run": r.id, "pane": r.pane}), json!({"facet": "execution", "from": prev.as_str(), "to": "exited", "source": "process"}));
        tx.event(
            "agent.exited",
            json!({"run": r.id, "pane": r.pane}),
            json!({"reason": reason, "harness": r.harness}),
        );
        for it in core
            .model
            .interactions
            .iter()
            .filter(|i| i.run == r.id && i.status == InteractionStatus::Open)
        {
            let mut it = it.clone();
            it.status = InteractionStatus::Cancelled;
            tx.event(
                "interaction.cancelled",
                json!({"interaction": it.id, "pane": it.pane}),
                json!({"reason": "process_exited"}),
            );
            tx.interaction(it);
        }
        let mut i = self.inner.lock().unwrap();
        let ids: Vec<String> = i
            .gates
            .iter()
            .filter(|(_, g)| g.pane == r.pane)
            .map(|(k, _)| k.clone())
            .collect();
        for k in ids {
            if let Some(g) = i.gates.remove(&k) {
                let _ = g.tx.send(GateReply::NoDecision);
            }
        }
        tx.run(r);
    }

    /// Screen fallback (04 §9): evaluated on output, ≤ 10 Hz per pane, only adding information
    /// to structured state (§2.5 rule 3).
    pub fn on_screen(&self, server: &Arc<Server>, pane: &str) {
        {
            let mut i = self.inner.lock().unwrap();
            let last = i
                .screen_eval
                .entry(pane.to_string())
                .or_insert_with(|| Instant::now() - Duration::from_secs(1));
            if last.elapsed() < Duration::from_millis(100) {
                return;
            }
            *last = Instant::now();
        }
        let Some(run) = server.with_core(|c| c.run_for_pane(pane).cloned()) else {
            return;
        };
        // A headless pane shows Vibeke's own transcript, not the harness's screen.
        if headless::is_headless(&run) {
            return;
        }
        let Some(h) = Harness::from_id(&run.harness) else {
            return;
        };
        let Some(rt) = server.pane_rt(pane) else {
            return;
        };
        let text = {
            let sc = rt.screen.lock().unwrap();
            sc.engine.screen_text()
        };
        let m = screen::evaluate(h, &text);
        let structured = run.execution.source == StateSource::Structured
            && run.health != AdapterHealth::Disconnected;
        // Execution state from the screen only when no structured transport drives the run.
        if !structured
            && let Some((state, conf)) = m.state.clone()
            && state != run.execution.value
        {
            set_execution(server, &run.id, state, StateSource::Screen, conf, None);
        }
        let open_screen = server.with_core(|c| {
            c.model
                .interactions
                .iter()
                .find(|i| i.pane == pane && i.status == InteractionStatus::Open)
                .cloned()
        });
        // §2.5 rule 3: with a healthy structured transport, a screen dialog only becomes an
        // interaction if the transport hasn't reported one within the grace period.
        if structured && let (Some(d), None) = (&m.dialog, &open_screen) {
            let mut i = self.inner.lock().unwrap();
            let fresh = match i.screen_grace.get(pane) {
                Some((fp, at)) if *fp == d.fingerprint => at.elapsed() < SCREEN_GRACE,
                _ => {
                    i.screen_grace
                        .insert(pane.to_string(), (d.fingerprint.clone(), Instant::now()));
                    let (srv, pane2) = (server.clone(), pane.to_string());
                    tokio::spawn(async move {
                        tokio::time::sleep(SCREEN_GRACE + Duration::from_millis(150)).await;
                        srv.agents.on_screen(&srv, &pane2);
                    });
                    true
                }
            };
            if fresh {
                return;
            }
        } else if m.dialog.is_none() {
            self.inner.lock().unwrap().screen_grace.remove(pane);
        }
        match (&m.dialog, open_screen) {
            (Some(d), None) => {
                // Provisional interaction from the screen (§2.5 rule 3); raise disagreement if
                // structured state says otherwise.
                let mut c = server.core.lock().unwrap();
                let handle = c.next_interaction_handle();
                let it = Interaction {
                    id: ulid(),
                    handle,
                    run: run.id.clone(),
                    pane: pane.to_string(),
                    kind: d.kind,
                    status: InteractionStatus::Open,
                    title: d.title.clone(),
                    body_md: None,
                    action: d.command.as_ref().map(|cmd| ActionInfo {
                        tool: d.tool.clone().unwrap_or_else(|| "Bash".into()),
                        summary: cmd.lines().next().unwrap_or("").to_string(),
                        command: Some(cmd.clone()),
                        paths: vec![],
                        diff: None,
                        risk: harness::risk(d.tool.as_deref().unwrap_or("Bash"), Some(cmd), &[]).0,
                        risk_reasons: harness::risk(
                            d.tool.as_deref().unwrap_or("Bash"),
                            Some(cmd),
                            &[],
                        )
                        .1,
                    }),
                    questions: d.options_as_question(),
                    plan_md: None,
                    answer_channel: AnswerChannel::Keystrokes,
                    native_ref: Some(format!("screen:{}", d.fingerprint)),
                    source: StateSource::Screen,
                    confidence: d.confidence,
                    answerable: !d.options.is_empty(),
                    gate: false,
                    decision_rev: 0,
                    delivery: DeliveryState::None,
                    delivery_error: None,
                    answer: None,
                    answered_by: None,
                    answer_key: None,
                    opened_at_ms: now_ms(),
                    answered_at_ms: None,
                };
                let mut tx = Tx::new();
                tx.counters = true;
                tx.event("interaction.opened", json!({"interaction": it.id, "pane": pane, "run": run.id}), json!({"kind": it.kind.as_str(), "source": "screen", "confidence": d.confidence}));
                if structured {
                    tx.event("adapter.disagreement", json!({"run": run.id}), json!({"facet": "interaction", "structured": run.execution.value.as_str(), "other": "dialog"}));
                }
                tx.interaction(it.clone());
                let _ = server.commit(&mut c, tx);
                drop(c);
                notify_interaction(server, &it, &run);
            }
            (None, Some(it)) if it.source == StateSource::Screen => {
                resolve(
                    server,
                    &it.id,
                    InteractionStatus::ResolvedElsewhere,
                    "dialog closed",
                );
            }
            _ => {}
        }
    }
}

fn new_run(
    c: &mut Core,
    pane: &str,
    h: Harness,
    integration: &str,
    source: StateSource,
    conf: f32,
) -> AgentRun {
    let handle = c.next_run_handle();
    let cwd = c.pane(pane).and_then(|p| p.cwd.clone());
    AgentRun {
        id: ulid(),
        handle,
        name: None,
        pane: pane.to_string(),
        harness: h.id().into(),
        harness_version: None,
        integration: integration.into(),
        harness_session_id: None,
        transcript_path: None,
        resume_argv: vec![],
        cwd,
        model: None,
        task: c
            .pane(pane)
            .and_then(|p| c.ws(&p.workspace))
            .and_then(|w| w.task.clone()),
        execution: facet(Execution::Starting, source, conf),
        health: AdapterHealth::Healthy,
        yolo: false,
        permission_mode: None,
        last_message: None,
        last_tool: None,
        turns_completed: 0,
        done_rev: 0,
        started_at_ms: now_ms(),
        ended_at_ms: None,
        capabilities: h.capabilities().iter().map(|s| s.to_string()).collect(),
        usage: Default::default(),
        rate_limit: None,
    }
}

fn update_run(server: &Server, run: &str, f: impl FnOnce(&mut AgentRun, &mut Tx)) {
    let mut c = server.core.lock().unwrap();
    let Some(mut r) = c.run(run).cloned() else {
        return;
    };
    let mut tx = Tx::new();
    f(&mut r, &mut tx);
    tx.run(r);
    let _ = server.commit(&mut c, tx);
}

fn set_execution(
    server: &Server,
    run: &str,
    to: Execution,
    source: StateSource,
    conf: f32,
    detail: Option<String>,
) {
    update_run(server, run, |r, tx| {
        if r.execution.value == to && r.execution.source == source {
            return;
        }
        let from = r.execution.value.clone();
        // Done marker: idle after work bumps done_rev (rendered as ✓ until seen, 08 §2.2).
        if to == Execution::Idle
            && matches!(
                from,
                Execution::Working | Execution::Starting | Execution::Unknown
            )
            && from != Execution::Starting
        {
            r.done_rev += 1;
            r.turns_completed += 1;
        }
        r.execution = Facet {
            detail,
            ..facet(to.clone(), source, conf)
        };
        let src = match source {
            StateSource::Structured => "structured",
            StateSource::Screen => "screen",
            StateSource::Process => "process",
            StateSource::SelfReport => "self_report",
            StateSource::User => "user",
        };
        tx.event("agent.state_changed", json!({"run": r.id, "pane": r.pane}), json!({"facet": "execution", "from": from.as_str(), "to": to.as_str(), "source": src, "confidence": conf}));
    });
}

fn notify_interaction(server: &Server, it: &Interaction, run: &AgentRun) {
    let who = run.name.clone().unwrap_or_else(|| run.harness.clone());
    let what = match it.kind {
        InteractionKind::Approval => "needs approval",
        InteractionKind::Question => "has a question",
        InteractionKind::PlanReview => "wants a plan review",
        InteractionKind::Notice => "notice",
    };
    let urgency = match it.action.as_ref().map(|a| a.risk) {
        Some(Risk::High) => "high",
        _ => "normal",
    };
    server.notify(
        "interaction",
        Some(&it.pane),
        &format!("{who} {what}"),
        &it.title,
        urgency,
    );
}

/// Close an interaction (resolved elsewhere / cancelled / expired) and drop any held gate.
fn resolve(server: &Server, id: &str, status: InteractionStatus, reason: &str) {
    if let Some(g) = server.agents.inner.lock().unwrap().gates.remove(id) {
        let _ = g.tx.send(GateReply::NoDecision);
    }
    let mut c = server.core.lock().unwrap();
    let Some(mut it) = c.interaction(id).cloned() else {
        return;
    };
    if it.status != InteractionStatus::Open && it.delivery != DeliveryState::Delivering {
        return;
    }
    let event = if it.delivery == DeliveryState::Delivering {
        // The harness moved on after our decision: native confirmation of delivery.
        it.delivery = DeliveryState::Delivered;
        "interaction.delivered"
    } else {
        it.status = status;
        if status == InteractionStatus::ResolvedElsewhere {
            it.delivery = DeliveryState::ResolvedElsewhere;
        }
        match status {
            InteractionStatus::Cancelled => "interaction.cancelled",
            InteractionStatus::Expired => "interaction.expired",
            _ => "interaction.resolved_elsewhere",
        }
    };
    let mut tx = Tx::new();
    tx.event(
        event,
        json!({"interaction": it.id, "pane": it.pane, "run": it.run}),
        json!({"reason": reason}),
    );
    tx.interaction(it);
    let _ = server.commit(&mut c, tx);
}

/// Hold a gate for an Interaction answered outside the hook path (sandbox egress, 13 §7).
/// Resolves `true` once `interaction.answer` recorded a decision, `false` when it was closed.
/// The gate is keyed to a pseudo-pane so release-on-focus and run end never drop it.
pub(crate) fn hold_external_gate(server: &Server, id: &str, pane: &str) -> oneshot::Receiver<bool> {
    let (tx, rx) = oneshot::channel();
    let (gtx, grx) = oneshot::channel::<GateReply>();
    server.agents.inner.lock().unwrap().gates.insert(
        id.to_string(),
        Gate {
            tx: gtx,
            pane: format!("external:{pane}"),
        },
    );
    tokio::spawn(async move {
        let answered = matches!(grx.await, Ok(GateReply::Decision { .. }));
        let _ = tx.send(answered);
    });
    rx
}

/// Close an Interaction from outside this module (confirms delivery when one is in flight).
pub(crate) fn close_interaction(
    server: &Server,
    id: &str,
    status: InteractionStatus,
    reason: &str,
) {
    resolve(server, id, status, reason);
}

// ---- hook transport ---------------------------------------------------------------------------

/// Bind the signal to a run in the caller's pane (deterministic binding via pane token, §2.6).
/// Detect the harness version once (off the state path) and gate capabilities (04 §12.3).
fn check_version(server: &Arc<Server>, run_id: &str, h: Harness) {
    let cached = server
        .agents
        .inner
        .lock()
        .unwrap()
        .versions
        .get(h.id())
        .cloned();
    let srv = server.clone();
    let run_id = run_id.to_string();
    tokio::spawn(async move {
        let v = match cached {
            Some(v) => v,
            None => {
                let v = tokio::task::spawn_blocking(move || harness::version(h))
                    .await
                    .ok()
                    .flatten();
                srv.agents
                    .inner
                    .lock()
                    .unwrap()
                    .versions
                    .insert(h.id().to_string(), v.clone());
                v
            }
        };
        let Some(v) = v else { return };
        let ok = harness::validated(h, &v);
        update_run(&srv, &run_id, |r, tx| {
            if r.harness_version.as_deref() == Some(v.as_str()) {
                return;
            }
            r.harness_version = Some(v.clone());
            if !ok {
                r.health = AdapterHealth::UnvalidatedVersion;
                r.execution.confidence = r.execution.confidence.min(0.8);
                r.capabilities = vec!["observe".into(), "answer_keystroke".into()];
                tx.event(
                    "agent.harness_version_unvalidated",
                    json!({"run": r.id, "pane": r.pane}),
                    json!({"harness": h.id(), "version": v}),
                );
            }
        });
    });
}

fn bound_run(server: &Arc<Server>, pane: &str, h: Harness) -> AgentRun {
    let mut c = server.core.lock().unwrap();
    if let Some(r) = c.run_for_pane(pane).cloned() {
        if r.harness == h.id() {
            // A structured transport proves health, but never lifts version gating (04 §12.3).
            let want = if r.health == AdapterHealth::UnvalidatedVersion {
                AdapterHealth::UnvalidatedVersion
            } else {
                AdapterHealth::Healthy
            };
            // A headless run keeps its protocol transport (its signals come from the adapter).
            let transport = if headless::is_headless(&r) {
                r.integration.clone()
            } else {
                h.transport().to_string()
            };
            if r.integration != transport || r.health != want {
                let mut r2 = r.clone();
                r2.integration = transport;
                r2.health = want;
                let mut tx = Tx::new();
                tx.event(
                    "adapter.health_changed",
                    json!({"run": r2.id}),
                    json!({"to": "healthy", "transport": r2.integration}),
                );
                tx.run(r2.clone());
                let _ = server.commit(&mut c, tx);
                return r2;
            }
            return r;
        }
        let mut tx = Tx::new();
        server.agents.end_run_tx(&mut c, &mut tx, &r, "replaced");
        let _ = server.commit(&mut c, tx);
    }
    let run = new_run(&mut c, pane, h, h.transport(), StateSource::Structured, 1.0);
    let mut tx = Tx::new();
    tx.counters = true;
    tx.event(
        "agent.started",
        json!({"run": run.id, "pane": pane}),
        json!({"harness": h.id(), "via": h.transport()}),
    );
    tx.run(run.clone());
    let _ = server.commit(&mut c, tx);
    drop(c);
    check_version(server, &run.id, h);
    run
}

/// pi/omp extension signals (integrations/pi-extension/PROTOCOL.md) onto the hook vocabulary.
fn on_extension_signal(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    let tool_name = |t: &str| match t {
        "write" => "Write".to_string(),
        "edit" | "multi_edit" => "Edit".to_string(),
        "bash" => "Bash".to_string(),
        other => other.to_string(),
    };
    let call = p.get("call_id").cloned().unwrap_or(Value::Null);
    match event {
        "SessionStart" => on_signal(server, pane, h, "SessionStart", p),
        "TurnStarted" => on_signal(
            server,
            pane,
            h,
            "UserPromptSubmit",
            // A preview-only payload (older extensions) is marked truncated, never "verbatim".
            &json!({"prompt": p.get("prompt").or_else(|| p.get("prompt_preview")), "prompt_truncated": p.get("prompt").is_none() || p.get("prompt_truncated").and_then(Value::as_bool).unwrap_or(false)}),
        ),
        "Working" | "Settling" => {
            let run = bound_run(server, pane, h);
            set_execution(
                server,
                &run.id,
                Execution::Working,
                StateSource::Structured,
                1.0,
                (event == "Settling").then(|| "settling".to_string()),
            );
        }
        "TurnEnded" => on_signal(
            server,
            pane,
            h,
            "Stop",
            &json!({"last_assistant_message": p.get("last_message")}),
        ),
        "ToolStarted" => {
            let t = tool_name(p.get("tool").and_then(Value::as_str).unwrap_or(""));
            on_signal(
                server,
                pane,
                h,
                "PreToolUse",
                &json!({"tool_name": t, "tool_input": p.get("input"), "tool_use_id": call}),
            );
        }
        "ToolEnded" => {
            let t = tool_name(p.get("tool").and_then(Value::as_str).unwrap_or(""));
            let ev = if p.get("ok").and_then(Value::as_bool).unwrap_or(true) {
                "PostToolUse"
            } else {
                "PostToolUseFailure"
            };
            on_signal(
                server,
                pane,
                h,
                ev,
                &json!({"tool_name": t, "tool_use_id": call, "tool_input": {"file_path": p.get("file_path"), "command": p.get("command")}, "exit_code": p.get("exit_code")}),
            );
        }
        "Error" => {
            let msg = p.get("message").and_then(Value::as_str).unwrap_or("error");
            if p.get("rate_limited")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                on_signal(
                    server,
                    pane,
                    h,
                    "StopFailure",
                    &json!({"error_type": "rate_limit"}),
                );
            } else if !p.get("retrying").and_then(Value::as_bool).unwrap_or(false) {
                on_signal(server, pane, h, "StopFailure", &json!({"error_type": msg}));
            }
        }
        "Compacting" => {
            let ev = if p.get("phase").and_then(Value::as_str) == Some("end") {
                "PostCompact"
            } else {
                "PreCompact"
            };
            on_signal(server, pane, h, ev, p);
        }
        "SessionEnded" => on_signal(server, pane, h, "SessionEnd", p),
        "Usage" => {
            let run = bound_run(server, pane, h);
            usage::from_extension(server, &run, p);
        }
        "ApprovalRequested" => {
            // omp's own approval dialog: observe only; answering happens in the pane (or via
            // the dialog bridge when omp routes it through uiContext).
            let run = bound_run(server, pane, h);
            let native = call.as_str().map(str::to_string);
            if server.with_core(|c| {
                c.model.interactions.iter().any(|i| {
                    i.run == run.id && i.native_ref == native && i.status == InteractionStatus::Open
                })
            }) {
                return;
            }
            let tool = p
                .get("tool")
                .and_then(Value::as_str)
                .unwrap_or("tool")
                .to_string();
            let reason = p.get("reason").and_then(Value::as_str).map(str::to_string);
            let mut c = server.core.lock().unwrap();
            let handle = c.next_interaction_handle();
            let mut it = harness::interaction_from_hook(
                Harness::Claude,
                "PermissionRequest",
                &json!({"tool_name": tool, "tool_input": {"command": reason}}),
            )
            .expect("approval");
            it.handle = handle;
            it.run = run.id.clone();
            it.pane = pane.to_string();
            it.native_ref = native;
            it.answerable = false;
            it.answer_channel = AnswerChannel::None;
            let mut tx = Tx::new();
            tx.counters = true;
            tx.event("interaction.opened", json!({"interaction": it.id, "pane": pane, "run": run.id}), json!({"kind": "approval", "source": "structured", "harness": h.id(), "answerable": false}));
            tx.interaction(it.clone());
            let _ = server.commit(&mut c, tx);
            drop(c);
            notify_interaction(server, &it, &run);
        }
        "ApprovalResolved" | "DialogResolved" => {
            let run = bound_run(server, pane, h);
            let key = p
                .get("call_id")
                .or_else(|| p.get("dialog_id"))
                .and_then(Value::as_str)
                .map(str::to_string);
            let id = server.with_core(|c| {
                c.model
                    .interactions
                    .iter()
                    .find(|i| i.run == run.id && i.native_ref == key)
                    .map(|i| i.id.clone())
            });
            if let Some(id) = id {
                resolve(
                    server,
                    &id,
                    InteractionStatus::ResolvedElsewhere,
                    "answered in pane",
                );
            }
        }
        "Snapshot" => {
            // Reconnect repair (DESIGN §2): replace our view of the run.
            if let Some(sid) = p.get("session_id").and_then(Value::as_str) {
                on_signal(
                    server,
                    pane,
                    h,
                    "SessionStart",
                    &json!({"session_id": sid, "transcript_path": p.get("session_file"), "model": p.get("model")}),
                );
            }
            let run = bound_run(server, pane, h);
            let streaming = p
                .get("is_streaming")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            set_execution(
                server,
                &run.id,
                if streaming {
                    Execution::Working
                } else {
                    Execution::Idle
                },
                StateSource::Structured,
                1.0,
                None,
            );
            let mut open: Vec<String> = p
                .get("open_approvals")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|x| {
                            x.get("call_id").and_then(Value::as_str).map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            // Newer extensions list wrapper dialogs still pending; then dialogs not listed were
            // handled elsewhere. Without the list (older extensions) dialogs are left alone.
            let dialogs: Option<Vec<Value>> =
                p.get("pending_dialogs").and_then(Value::as_array).cloned();
            if let Some(d) = &dialogs {
                open.extend(d.iter().filter_map(|x| {
                    x.get("dialog_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }));
            }
            let has_dialog_list = dialogs.is_some();
            let stale: Vec<String> = server.with_core(|c| {
                c.model
                    .interactions
                    .iter()
                    .filter(|i| {
                        i.run == run.id
                            && i.status == InteractionStatus::Open
                            && i.native_ref.as_ref().is_some_and(|r| !open.contains(r))
                            && (i.answer_channel == AnswerChannel::None || has_dialog_list)
                    })
                    .map(|i| i.id.clone())
                    .collect()
            });
            for id in stale {
                resolve(
                    server,
                    &id,
                    InteractionStatus::ResolvedElsewhere,
                    "not pending after reconnect",
                );
            }
            for a in p
                .get("open_approvals")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
            {
                on_extension_signal(server, pane, h, "ApprovalRequested", &a);
            }
            // Pending dialogs we don't know (opened while disconnected): observe-only.
            for d in dialogs.unwrap_or_default() {
                let Some(id) = d.get("dialog_id").and_then(Value::as_str) else {
                    continue;
                };
                let known = server.with_core(|c| {
                    c.model.interactions.iter().any(|i| {
                        i.run == run.id
                            && i.native_ref.as_deref() == Some(id)
                            && i.status == InteractionStatus::Open
                    })
                });
                if known {
                    continue;
                }
                let Some(mut it) = harness::interaction_from_hook(h, "Dialog", &d) else {
                    continue;
                };
                let mut c = server.core.lock().unwrap();
                it.handle = c.next_interaction_handle();
                it.run = run.id.clone();
                it.pane = pane.to_string();
                it.answerable = false;
                it.answer_channel = AnswerChannel::None;
                let mut tx = Tx::new();
                tx.event(
                    "interaction.opened",
                    json!({"interaction": it.id, "run": run.id}),
                    json!({"kind": it.kind.as_str(), "observe_only": true, "via": "snapshot"}),
                );
                tx.interaction(it);
                let _ = server.commit(&mut c, tx);
            }
        }
        _ => {}
    }
}

fn on_signal(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    let run = bound_run(server, pane, h);
    // SessionStart is observed after the run's session id is updated below, so a binding never
    // appears suspended while the run still reports the old conversation.
    if event != "SessionStart" {
        crate::tracking::observe(server, &run, event, p);
    }
    usage::observe(server, &run, event, p);
    crate::items::observe_hook(server, &run, event, p);
    let sid = p.get("session_id").and_then(Value::as_str);
    let tool_use = p.get("tool_use_id").and_then(Value::as_str);
    if let Some(mode) = p.get("permission_mode").and_then(Value::as_str)
        && run.permission_mode.as_deref() != Some(mode)
    {
        let yolo = mode == "bypassPermissions" || run.yolo;
        update_run(server, &run.id, |r, _| {
            r.permission_mode = Some(mode.to_string());
            r.yolo = yolo;
        });
    }
    match event {
        "SessionStart" => {
            let transcript = p
                .get("transcript_path")
                .and_then(Value::as_str)
                .map(str::to_string);
            let model = p.get("model").and_then(Value::as_str).map(str::to_string);
            let cwd = p.get("cwd").and_then(Value::as_str).map(str::to_string);
            update_run(server, &run.id, |r, tx| {
                if let Some(s) = sid
                    && r.harness_session_id.as_deref() != Some(s)
                {
                    r.harness_session_id = Some(s.to_string());
                    r.resume_argv = h.resume_argv(s);
                    tx.event(
                        "agent.identified",
                        json!({"run": r.id, "pane": r.pane}),
                        json!({"harness_session_id": s, "transcript_path": transcript}),
                    );
                    tx.event(
                        "agent.resume_handle",
                        json!({"run": r.id}),
                        json!({"argv": r.resume_argv}),
                    );
                }
                r.transcript_path = transcript.clone().or(r.transcript_path.take());
                r.model = model.clone().or(r.model.take());
                r.cwd = cwd.clone().or(r.cwd.take());
            });
            set_execution(
                server,
                &run.id,
                Execution::Idle,
                StateSource::Structured,
                1.0,
                None,
            );
            crate::tracking::observe(server, &run, event, p);
        }
        "UserPromptSubmit" => {
            set_execution(
                server,
                &run.id,
                Execution::Working,
                StateSource::Structured,
                1.0,
                None,
            );
            // Events carry metadata only (15 §10.3, 09): never prompt text, which may hold secrets.
            let prompt = p.get("prompt").and_then(Value::as_str).unwrap_or("");
            let meta = json!({"prompt_bytes": prompt.len(), "prompt_digest": blake3::hash(prompt.as_bytes()).to_hex()[..16].to_string()});
            update_run(server, &run.id, |r, tx| {
                tx.event(
                    "agent.turn_started",
                    json!({"run": r.id, "pane": r.pane}),
                    meta.clone(),
                );
            });
        }
        "PreToolUse" => {
            set_execution(
                server,
                &run.id,
                Execution::Working,
                StateSource::Structured,
                1.0,
                None,
            );
            let tool = p
                .get("tool_name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let summary = harness::tool_summary(&tool, p.get("tool_input").unwrap_or(&Value::Null));
            update_run(server, &run.id, |r, _| r.last_tool = Some(summary));
        }
        "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" => {
            if let Some(t) = tool_use {
                let open = server.with_core(|c| {
                    c.model
                        .interactions
                        .iter()
                        .find(|i| i.run == run.id && i.native_ref.as_deref() == Some(t))
                        .map(|i| i.id.clone())
                });
                if let Some(id) = open {
                    resolve(server, &id, InteractionStatus::ResolvedElsewhere, event);
                }
            }
            if event != "PermissionDenied" {
                let tool = p.get("tool_name").and_then(Value::as_str).unwrap_or("");
                if matches!(tool, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
                    && let Some(path) = p.pointer("/tool_input/file_path").and_then(Value::as_str)
                {
                    update_run(server, &run.id, |r, tx| {
                        tx.event("agent.file_changed", json!({"run": r.id, "pane": r.pane}), json!({"path": path, "op": if tool == "Write" { "create" } else { "modify" }}));
                    });
                }
            }
            // Screen-provisional dialogs are resolved once tools run again.
            let screen_open: Vec<String> = server.with_core(|c| {
                c.model
                    .interactions
                    .iter()
                    .filter(|i| {
                        i.run == run.id
                            && i.status == InteractionStatus::Open
                            && i.source == StateSource::Screen
                    })
                    .map(|i| i.id.clone())
                    .collect()
            });
            for id in screen_open {
                resolve(
                    server,
                    &id,
                    InteractionStatus::ResolvedElsewhere,
                    "tool ran",
                );
            }
        }
        "Stop" | "Interrupt" => {
            let msg = p
                .get("last_assistant_message")
                .and_then(Value::as_str)
                .map(str::to_string)
                .or_else(|| last_assistant_message(p));
            update_run(server, &run.id, |r, tx| {
                if let Some(m) = &msg {
                    r.last_message = Some(m.chars().take(2000).collect());
                }
                tx.event(
                    "agent.turn_completed",
                    json!({"run": r.id, "pane": r.pane}),
                    json!({"stop_reason": event}),
                );
            });
            set_execution(
                server,
                &run.id,
                Execution::Idle,
                StateSource::Structured,
                1.0,
                None,
            );
            // A turn ending resolves its open interactions (§2.5 rule 2).
            let open: Vec<String> = server.with_core(|c| {
                c.model
                    .interactions
                    .iter()
                    .filter(|i| i.run == run.id && i.status == InteractionStatus::Open)
                    .map(|i| i.id.clone())
                    .collect()
            });
            for id in open {
                resolve(
                    server,
                    &id,
                    InteractionStatus::ResolvedElsewhere,
                    "turn ended",
                );
            }
            let who = run.name.clone().unwrap_or_else(|| run.harness.clone());
            if !server.pane_focused_by_any(pane) {
                server.notify(
                    "agent_state",
                    Some(pane),
                    &format!("{who} is done"),
                    msg.as_deref().unwrap_or(""),
                    "low",
                );
            }
        }
        "StopFailure" => {
            let kind = p
                .get("error_type")
                .or_else(|| p.get("matcher"))
                .or_else(|| p.get("reason"))
                .and_then(Value::as_str)
                .unwrap_or("error")
                .to_string();
            let to = if kind.contains("rate") {
                Execution::RateLimited
            } else {
                Execution::Error
            };
            set_execution(
                server,
                &run.id,
                to,
                StateSource::Structured,
                1.0,
                Some(kind),
            );
        }
        "Notification" => {
            let ty = p
                .get("notification_type")
                .or_else(|| p.get("matcher"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let message = p.get("message").and_then(Value::as_str).unwrap_or("");
            if ty == "idle_prompt" || message.contains("waiting for your input") {
                set_execution(
                    server,
                    &run.id,
                    Execution::Idle,
                    StateSource::Structured,
                    1.0,
                    None,
                );
            } else if ty.starts_with("quota_auto_resume") {
                set_execution(
                    server,
                    &run.id,
                    Execution::RateLimited,
                    StateSource::Structured,
                    1.0,
                    None,
                );
            }
        }
        "PreCompact" => update_run(server, &run.id, |r, _| {
            r.execution.detail = Some("compacting".into())
        }),
        "PostCompact" => update_run(server, &run.id, |r, _| r.execution.detail = None),
        "SessionEnd" => {
            let reason = p.get("reason").and_then(Value::as_str).unwrap_or("");
            if reason != "clear" {
                update_run(server, &run.id, |r, tx| {
                    tx.event(
                        "agent.session_ended",
                        json!({"run": r.id, "pane": r.pane}),
                        json!({"reason": reason}),
                    );
                });
            }
        }
        _ => {}
    }
}

fn last_assistant_message(p: &Value) -> Option<String> {
    let path = p.get("transcript_path")?.as_str()?;
    harness::transcript_last_message(std::path::Path::new(path))
}

/// `adapter.gate`: open an interaction and either answer by policy, hold until a client
/// decides (gate mode), or return at once so the native dialog shows (observe mode).
async fn gate(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) -> R {
    let run = bound_run(server, pane, h);
    let Some(mut it) = harness::interaction_from_hook(h, event, p) else {
        return Ok(json!({"decision": null}));
    };
    it.run = run.id.clone();
    it.pane = pane.to_string();
    // Questions are answered natively only where the capability is verified (04 §2.3).
    let validated = run.health != AdapterHealth::UnvalidatedVersion;
    let native = validated && h.answer_native(it.kind);
    // Policy fast path (02 §4): only for approvals the harness lets us gate.
    let policy = if it.kind == InteractionKind::Approval && native {
        match_policy(server, &it)
    } else {
        None
    };
    let focused = server.pane_focused_by_any(pane);
    let gate_mode = native && (policy.is_some() || !focused);
    it.gate = gate_mode;
    it.answer_channel = if native {
        AnswerChannel::Native
    } else {
        AnswerChannel::Keystrokes
    };
    it.answerable = native || h.keystroke_answers(it.kind);
    let (tx_reply, rx_reply) = oneshot::channel::<GateReply>();
    {
        let mut c = server.core.lock().unwrap();
        it.handle = c.next_interaction_handle();
        // Re-attach by native ref (a hook retried after a dropped connection, 04 §7.3 rule 4):
        // an already-decided interaction returns its recorded decision; never reopen it.
        if let Some(existing) = c
            .model
            .interactions
            .iter()
            .find(|x| x.native_ref.is_some() && x.native_ref == it.native_ref && x.run == run.id)
            .cloned()
        {
            if existing.status == InteractionStatus::Answered
                && let Some(ans) = existing.answer.clone()
            {
                let json = harness::decision_json(h, &existing, &ans);
                let key = format!("{}:{}", existing.id, existing.decision_rev);
                return Ok(
                    json!({"decision": json, "interaction": existing.id, "idempotency_key": key, "resumed": true}),
                );
            }
            it.id = existing.id.clone();
            it.handle = existing.handle.clone();
        }
        let mut tx = Tx::new();
        tx.counters = true;
        tx.event(
            "interaction.opened",
            json!({"interaction": it.id, "pane": pane, "run": run.id}),
            json!({"kind": it.kind.as_str(), "source": "structured", "confidence": 1.0, "gate": gate_mode, "native_ref": it.native_ref, "risk": it.action.as_ref().map(|a| format!("{:?}", a.risk).to_lowercase())}),
        );
        tx.interaction(it.clone());
        server.commit(&mut c, tx).map_err(internal)?;
    }
    if let Some((effect, rule)) = policy {
        let decision = if effect == "allow" {
            Decision::Allow
        } else {
            Decision::Deny
        };
        let answer = Answer {
            decision: Some(decision),
            choices: vec![],
            text: Some(format!("{effect} by policy rule {rule}")),
        };
        let (json, key) = record_decision(server, &it.id, answer, "policy", None, None, None)
            .map_err(|e| err(ErrorKind::Conflict, e))?
            .unwrap_or_default();
        return Ok(json!({"decision": json, "interaction": it.id, "idempotency_key": key}));
    }
    notify_interaction(server, &it, &run);
    if !gate_mode {
        return Ok(json!({"decision": null, "interaction": it.id, "mode": "observe"}));
    }
    server.agents.inner.lock().unwrap().gates.insert(
        it.id.clone(),
        Gate {
            tx: tx_reply,
            pane: pane.to_string(),
        },
    );
    let reply = tokio::time::timeout(GATE_TIMEOUT, rx_reply).await;
    match reply {
        Ok(Ok(GateReply::Decision { json, key })) => {
            Ok(json!({"decision": json, "interaction": it.id, "idempotency_key": key}))
        }
        Ok(Ok(GateReply::NoDecision)) | Ok(Err(_)) => {
            Ok(json!({"decision": null, "interaction": it.id}))
        }
        Err(_) => {
            server.agents.inner.lock().unwrap().gates.remove(&it.id);
            // Harness-backed approval: no decision → the native dialog appears (§2.7).
            let mut c = server.core.lock().unwrap();
            if let Some(mut x) = c.interaction(&it.id).cloned() {
                x.gate = false;
                let mut tx = Tx::new();
                tx.event(
                    "interaction.updated",
                    json!({"interaction": x.id}),
                    json!({"gate": false, "reason": "gate_timeout"}),
                );
                tx.interaction(x);
                let _ = server.commit(&mut c, tx);
            }
            Ok(json!({"decision": null, "interaction": it.id}))
        }
    }
}

fn match_policy(server: &Server, it: &Interaction) -> Option<(String, String)> {
    // Config, API-added and trusted repository rules (07 §2.9, 09 §4).
    crate::policy_api::match_interaction(server, it)
}

/// Step 1 of the delivery transaction (02 §1.1): record the decision (first writer wins).
/// Returns the native hook JSON and the idempotency key, or `None` when this exact answer
/// (same idempotency key) was already recorded and must not be delivered again.
fn record_decision(
    server: &Server,
    id: &str,
    answer: Answer,
    by: &str,
    actor: Option<&str>,
    idem: Option<&str>,
    expected_rev: Option<u32>,
) -> Result<Option<(Value, String)>, String> {
    let mut c = server.core.lock().unwrap();
    let Some(mut it) = c.interaction(id).cloned() else {
        return Err("interaction not found".into());
    };
    // Compare-and-set under the core lock: the caller decided on this revision.
    if it.status == InteractionStatus::Open && expected_rev.is_some_and(|r| r != it.decision_rev) {
        return Err(format!(
            "stale: interaction is at revision {}",
            it.decision_rev
        ));
    }
    let key = format!("{}:{}", it.id, it.decision_rev + 1);
    if it.status != InteractionStatus::Open {
        // Records from before `answer_key` kept the key in `answered_by`.
        let recorded = it.answer_key.as_deref().or(it
            .answered_by
            .as_deref()
            .filter(|_| it.answer_key.is_none()));
        if idem.is_some() && recorded == idem {
            return if it.answer.as_ref() == Some(&answer) {
                Ok(None)
            } else {
                Err("idempotency key reused with a different answer".into())
            };
        }
        return Err(format!(
            "already answered by {}",
            it.answered_by
                .clone()
                .unwrap_or_else(|| "someone else".into())
        ));
    }
    let h = c.run(&it.run).and_then(|r| Harness::from_id(&r.harness));
    let native = h
        .map(|h| harness::decision_json(h, &it, &answer))
        .unwrap_or(Value::Null);
    it.status = InteractionStatus::Answered;
    it.decision_rev += 1;
    it.delivery = DeliveryState::DecisionRecorded;
    it.answer = Some(answer.clone());
    it.answered_by = Some(
        actor
            .or(idem)
            .map(str::to_string)
            .unwrap_or_else(|| by.to_string()),
    );
    it.answer_key = idem.map(str::to_string);
    it.answered_at_ms = Some(now_ms());
    let mut tx = Tx::new();
    tx.event_by(
        "interaction.decided",
        json!({"interaction": it.id, "pane": it.pane, "run": it.run}),
        json!({"kind": by}),
        json!({"rev": it.decision_rev, "by": by, "decision": answer.decision.map(|d| format!("{d:?}").to_lowercase()), "channel": format!("{:?}", it.answer_channel).to_lowercase()}),
    );
    if by == "policy" {
        tx.event(
            "policy.rule_matched",
            json!({"interaction": it.id}),
            json!({"effect": answer.decision.map(|d| format!("{d:?}").to_lowercase())}),
        );
    }
    let audited = it.clone();
    tx.interaction(it);
    server.commit(&mut c, tx).map_err(|e| format!("{e:#}"))?;
    crate::security::decision_recorded(server, &audited, by, actor);
    Ok(Some((native, key)))
}

fn set_delivery(server: &Server, id: &str, state: DeliveryState, error: Option<String>) {
    let mut c = server.core.lock().unwrap();
    let Some(mut it) = c.interaction(id).cloned() else {
        return;
    };
    if it.delivery == state {
        return;
    }
    it.delivery = state;
    it.delivery_error = error.clone();
    let ev = match state {
        DeliveryState::Delivering => "interaction.delivery_started",
        DeliveryState::Delivered => "interaction.delivered",
        DeliveryState::DeliveryUnknown => "interaction.delivery_unknown",
        DeliveryState::Failed => "interaction.delivery_failed",
        _ => "interaction.updated",
    };
    let mut tx = Tx::new();
    tx.event(
        ev,
        json!({"interaction": it.id, "pane": it.pane, "run": it.run}),
        json!({"reason": error}),
    );
    tx.interaction(it);
    let _ = server.commit(&mut c, tx);
}

/// `interaction.answer`: record, then deliver natively (held gate) or by verified keystrokes.
async fn answer(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = crate::api::req(p, "interaction")?;
    let it = server
        .with_core(|c| c.interaction(id).cloned())
        .ok_or_else(|| not_found("interaction", id))?;
    // Retrieving vs authorizing (09 §5.1.1): a pane may not answer its own interaction.
    if ctx.pane_scope.as_deref() == Some(it.pane.as_str()) {
        return Err(err(ErrorKind::PermissionDenied, "self_answer_forbidden")
            .details(json!({"interaction": it.handle})));
    }
    if !it.answerable {
        return Err(err(
            ErrorKind::Unsupported,
            "this dialog can only be answered in the pane",
        )
        .details(json!({"fallback": "focus the pane"})));
    }
    let decision = match s(p, "decision") {
        Some("allow") => Some(Decision::Allow),
        Some("allow_always") => Some(Decision::AllowAlways),
        Some("deny") => Some(Decision::Deny),
        Some(x) => return Err(invalid(format!("unknown decision {x}"))),
        None => None,
    };
    let mut choices = Vec::new();
    if let Some(m) = p.get("choices").and_then(Value::as_object) {
        for (q, v) in m {
            let opts: Vec<String> = match v {
                Value::Array(a) => a
                    .iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect(),
                Value::String(s) => vec![s.clone()],
                _ => vec![],
            };
            choices.push((q.clone(), opts));
        }
    }
    if decision.is_none() && choices.is_empty() && s(p, "text").is_none() {
        return Err(invalid("decision, choices or text required"));
    }
    let answer = Answer {
        decision,
        choices,
        text: s(p, "text").map(str::to_string),
    };
    let by = format!("{}:{}", ctx.kind, ctx.client_id);
    let idem = s(p, "idempotency_key");
    // A display label for the answerer (e.g. "gateway:the maintainer's phone"); full-scope clients only.
    let actor = s(p, "actor").filter(|_| ctx.pane_scope.is_none());
    let expected = p
        .get("expected_decision_rev")
        .and_then(Value::as_u64)
        .map(|r| r as u32);
    // Degraded storage: a decision that can't be recorded is never delivered (02 §4a).
    crate::hardening::refuse_answer(server)?;
    let Some((native, key)) =
        record_decision(server, id, answer.clone(), &by, actor, idem, expected).map_err(|e| {
            if e.contains("storage unavailable") {
                crate::hardening::answer_refusal(&e)
            } else {
                err(ErrorKind::Conflict, e)
            }
        })?
    else {
        // Retry of an answer already recorded: report state, never deliver twice.
        let it = server.with_core(|c| c.interaction(id).cloned());
        return Ok(
            json!({"interaction": it, "delivery": {"channel": "recorded"}, "duplicate": true}),
        );
    };
    let gate = server.agents.inner.lock().unwrap().gates.remove(&it.id);
    let channel;
    let headless = server.pane_rt(&it.pane).is_some_and(|rt| rt.is_pipe());
    if headless {
        // Native over the harness protocol: the pane's adapter writes the response and the
        // holder ack confirms delivery.
        channel = "native";
        if !headless::deliver_answer(server, &it, &key) {
            set_delivery(
                server,
                &it.id,
                DeliveryState::DeliveryUnknown,
                Some("headless pane not running".into()),
            );
        }
    } else if let Some(g) = gate {
        channel = "native";
        set_delivery(server, &it.id, DeliveryState::Delivering, None);
        if g.tx
            .send(GateReply::Decision { json: native, key })
            .is_err()
        {
            set_delivery(
                server,
                &it.id,
                DeliveryState::DeliveryUnknown,
                Some("hook disconnected".into()),
            );
        }
    } else {
        channel = "keystrokes";
        set_delivery(server, &it.id, DeliveryState::Delivering, None);
        let srv = server.clone();
        let iid = it.id.clone();
        tokio::spawn(async move {
            let (state, err) = deliver_keystrokes(&srv, &iid, &answer).await;
            set_delivery(&srv, &iid, state, err);
        });
    }
    let it = server.with_core(|c| c.interaction(id).cloned());
    Ok(json!({"interaction": it, "delivery": {"channel": channel}}))
}

/// Verified keystroke delivery (04 §8): best effort; `delivered` only when the dialog closes.
fn norm_ws(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Does the on-screen dialog describe this interaction?
fn dialog_matches(d: &screen::Dialog, it: &Interaction) -> bool {
    if let Some(fp) = it
        .native_ref
        .as_deref()
        .and_then(|r| r.strip_prefix("screen:"))
    {
        return fp == d.fingerprint;
    }
    if d.kind != it.kind
        && !(it.kind == InteractionKind::PlanReview && d.kind == InteractionKind::Approval)
    {
        return false;
    }
    match (
        it.action.as_ref().and_then(|a| a.command.as_deref()),
        d.command.as_deref(),
    ) {
        (Some(want), Some(got)) => {
            let (w, g) = (norm_ws(want.lines().next().unwrap_or(want)), norm_ws(got));
            !w.is_empty() && (g.contains(&w) || w.contains(&g)) && g.len() >= 3
        }
        (Some(_), None) => false,
        (None, _) => {
            // Questions: the prompt text must be on screen.
            let prompt = it
                .questions
                .first()
                .map(|q| norm_ws(&q.prompt))
                .unwrap_or_else(|| norm_ws(&it.title));
            let shown = norm_ws(&d.title);
            !prompt.is_empty() && (shown.contains(&prompt) || prompt.contains(&shown))
        }
    }
}

async fn deliver_keystrokes(
    server: &Arc<Server>,
    id: &str,
    answer: &Answer,
) -> (DeliveryState, Option<String>) {
    let Some(it) = server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .find(|i| i.id == id)
            .cloned()
            .or_else(|| {
                c.store
                    .find::<Interaction>("interaction", id)
                    .ok()
                    .flatten()
            })
    }) else {
        return (DeliveryState::Failed, Some("interaction gone".into()));
    };
    let Some(h) = server.with_core(|c| c.run(&it.run).and_then(|r| Harness::from_id(&r.harness)))
    else {
        return (DeliveryState::Failed, Some("run gone".into()));
    };
    let Some(rt) = server.pane_rt(&it.pane) else {
        return (DeliveryState::Failed, Some("pane gone".into()));
    };
    let screen_text = || rt.screen.lock().unwrap().engine.screen_text();
    let before = screen_text();
    let Some(dialog) = screen::evaluate(h, &before).dialog else {
        return (DeliveryState::Failed, Some("dialog_changed".into()));
    };
    // The visible dialog must be the one this interaction describes (04 §8 step 2); a stale
    // answer must never select an option in a replacement dialog.
    if !dialog_matches(&dialog, &it) {
        return (
            DeliveryState::Failed,
            Some("dialog_changed: the visible dialog is not this interaction".into()),
        );
    }
    let Some(keys) = screen::keys_for(h, &dialog, &it, answer) else {
        return (DeliveryState::Failed, Some("selection_mismatch".into()));
    };
    server
        .agents
        .inner
        .lock()
        .unwrap()
        .locks
        .insert(it.pane.clone(), Instant::now());
    let modes = rt.screen.lock().unwrap().engine.input_modes();
    let mut bytes = Vec::new();
    for k in &keys {
        if let Ok(ev) = vk_term::keygrammar::parse_key(k) {
            bytes.extend(vk_term::encode::encode_key(&ev, &modes));
        }
    }
    let status = rt.input(server.next_internal_input_id(), bytes).await;
    server.agents.inner.lock().unwrap().locks.remove(&it.pane);
    if status == vk_proto::holder::InputStatus::ChildExited {
        return (DeliveryState::Failed, Some("pane exited".into()));
    }
    // Confirm: the dialog disappears within 2 s.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
        let now = screen_text();
        match screen::evaluate(h, &now).dialog {
            None => return (DeliveryState::Delivered, None),
            Some(d) if d.fingerprint != dialog.fingerprint => {
                return (
                    DeliveryState::DeliveryUnknown,
                    Some("a different dialog appeared; check the pane".into()),
                );
            }
            Some(_) => {}
        }
    }
    (
        DeliveryState::DeliveryUnknown,
        Some("dialog still visible".into()),
    )
}

// ---- agent.* ------------------------------------------------------------------------------------

fn run_json(c: &Core, r: &AgentRun) -> Value {
    let mut v = serde_json::to_value(r).unwrap_or(Value::Null);
    let pane = c.pane(&r.pane);
    v["pane_handle"] = json!(pane.map(|p| p.handle.clone()));
    v["workspace"] = json!(
        pane.and_then(|p| c.ws(&p.workspace))
            .map(|w| w.display_name().to_string())
    );
    v["open_interactions"] = json!(
        c.model
            .interactions
            .iter()
            .filter(|i| i.run == r.id && i.status == InteractionStatus::Open)
            .count()
    );
    let seen = c
        .store
        .reads("local")
        .unwrap_or_default()
        .into_iter()
        .find(|(p, _)| p == &r.pane)
        .map(|(_, s)| s)
        .unwrap_or(0);
    v["done"] = json!(r.execution.value == Execution::Idle && r.done_rev > seen);
    v
}

fn resolve_run(
    server: &Server,
    ctx: &Ctx,
    target: Option<&str>,
) -> Result<AgentRun, vk_proto::rpc::RpcError> {
    if let Some(t) = target
        && let Some(r) = server.with_core(|c| c.run(t).cloned())
    {
        return Ok(r);
    }
    let pane = resolve_pane(server, ctx, target)?;
    server
        .with_core(|c| c.run_for_pane(&pane.id).cloned())
        .ok_or_else(|| not_found("run", target.unwrap_or("@current")))
}

fn validate_name(server: &Server, n: &str) -> Result<(), vk_proto::rpc::RpcError> {
    let valid = n.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && n.len() <= 32
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if !valid {
        return Err(invalid("agent names: [a-z][a-z0-9_-]{0,31}"));
    }
    if server.with_core(|c| c.model.runs.iter().any(|r| r.name.as_deref() == Some(n))) {
        return Err(err(ErrorKind::Conflict, "name_taken"));
    }
    Ok(())
}

pub async fn start_in_pane(
    server: &Arc<Server>,
    pane: &str,
    harness: &str,
    name: Option<&str>,
    prompt: Option<&str>,
    args: &[String],
    task: Option<&str>,
) -> Result<Value, vk_proto::rpc::RpcError> {
    let opts = crate::sandbox::LaunchOpts::default();
    start_in_pane_opts(server, pane, harness, name, prompt, args, task, &opts).await
}

/// [`start_in_pane`] with `--yolo` / `--isolate` / `--network` (13 §3).
#[allow(clippy::too_many_arguments)]
pub async fn start_in_pane_opts(
    server: &Arc<Server>,
    pane: &str,
    harness: &str,
    name: Option<&str>,
    prompt: Option<&str>,
    args: &[String],
    task: Option<&str>,
    opts: &crate::sandbox::LaunchOpts,
) -> Result<Value, vk_proto::rpc::RpcError> {
    let h =
        Harness::from_id(harness).ok_or_else(|| invalid(format!("unknown harness {harness}")))?;
    if let Some(n) = name {
        validate_name(server, n)?;
    }
    if server.with_core(|c| c.run_for_pane(pane).is_some()) {
        return Err(err(ErrorKind::Conflict, "pane_busy")
            .details(json!({"reason": "an agent already runs in this pane"})));
    }
    let session_id = h.preassign_session_id();
    let launch = crate::sandbox::prepare_agent(
        server,
        pane,
        h.id(),
        h.launch_argv(session_id.as_deref(), args, prompt),
        opts,
    )
    .await?;
    let argv = launch.argv;
    let run = {
        let mut c = server.core.lock().unwrap();
        let mut run = new_run(&mut c, pane, h, "process", StateSource::Process, 0.6);
        run.name = name.map(str::to_string);
        run.task = task.map(str::to_string).or(run.task);
        run.yolo = harness::yolo(h, &argv);
        if let Some(s) = &session_id {
            run.harness_session_id = Some(s.clone());
            run.resume_argv = h.resume_argv(s);
        }
        let mut tx = Tx::new();
        tx.counters = true;
        tx.event(
            "agent.started",
            json!({"run": run.id, "pane": pane}),
            json!({"harness": h.id(), "via": "vibeke", "argv": argv}),
        );
        tx.run(run.clone());
        server.commit(&mut c, tx).map_err(internal)?;
        run
    };
    let line = format!("{}\r", launch.line);
    crate::render::write_and_ack(
        server,
        pane,
        server.next_internal_input_id(),
        line.into_bytes(),
    )
    .await;
    // Ready when the harness reports itself (SessionStart) or its input box shows.
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        let st = server.with_core(|c| c.run(&run.id).map(|r| r.execution.value.clone()));
        match st {
            Some(Execution::Idle | Execution::Working) | None => break,
            _ => tokio::time::sleep(Duration::from_millis(200)).await,
        }
    }
    let r = server.with_core(|c| c.run(&run.id).map(|r| run_json(c, r)));
    Ok(json!({"run": r}))
}

async fn prompt(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "target"))?;
    let text = crate::api::req(p, "text")?;
    if headless::is_headless(&run) {
        let rev0 = run.turns_completed;
        headless::prompt(
            server,
            &run,
            text,
            headless::PromptMode::parse(s(p, "mode")),
        )
        .await?;
        if p.get("wait").and_then(Value::as_bool).unwrap_or(false) {
            let until = vec![
                "idle".to_string(),
                "needs_approval".into(),
                "needs_answer".into(),
                "error".into(),
                "exited".into(),
            ];
            return wait(
                server,
                &run,
                &until,
                u(p, "timeout_ms").unwrap_or(600_000),
                Some(rev0),
            )
            .await;
        }
        let r = server.with_core(|c| c.run(&run.id).map(|r| run_json(c, r)));
        return Ok(json!({"run": r}));
    }
    let modes = crate::render::input_modes(server, &run.pane);
    let mut m = modes;
    m.bracketed_paste = modes.bracketed_paste;
    let mut bytes = if modes.bracketed_paste {
        vk_term::encode::encode_paste(text, &m)
    } else {
        text.as_bytes().to_vec()
    };
    let was_working = run.execution.value == Execution::Working;
    let rev0 = run.turns_completed;
    crate::render::write_and_ack(
        server,
        &run.pane,
        server.next_internal_input_id(),
        std::mem::take(&mut bytes),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    crate::render::write_and_ack(
        server,
        &run.pane,
        server.next_internal_input_id(),
        b"\r".to_vec(),
    )
    .await;
    // Stall detection (07 §1.4): no lifecycle change within 5 s.
    if !was_working {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut started = false;
        while Instant::now() < deadline {
            let st = server.with_core(|c| {
                c.run(&run.id)
                    .map(|r| (r.execution.value.clone(), r.turns_completed))
            });
            if st
                .as_ref()
                .is_some_and(|(e, t)| *e == Execution::Working || *t > rev0)
            {
                started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if !started && run.integration == "hooks" {
            return Err(
                err(ErrorKind::Stalled, "agent_prompt_stalled").details(json!({"run": run.handle}))
            );
        }
    }
    if p.get("wait").and_then(Value::as_bool).unwrap_or(false) {
        let until = vec![
            "idle".to_string(),
            "needs_approval".into(),
            "needs_answer".into(),
            "error".into(),
            "exited".into(),
        ];
        return wait(
            server,
            &run,
            &until,
            u(p, "timeout_ms").unwrap_or(600_000),
            Some(rev0),
        )
        .await;
    }
    let r = server.with_core(|c| c.run(&run.id).map(|r| run_json(c, r)));
    Ok(json!({"run": r}))
}

async fn wait(
    server: &Arc<Server>,
    run: &AgentRun,
    until: &[String],
    timeout_ms: u64,
    turns_at_start: Option<u32>,
) -> R {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let start_turns = turns_at_start.unwrap_or(run.turns_completed);
    let mut rx = server.events.subscribe();
    loop {
        let (state, ints, turns, alive) = server.with_core(|c| match c.run(&run.id) {
            Some(r) => (
                r.execution.value.clone(),
                c.model
                    .interactions
                    .iter()
                    .filter(|i| i.run == r.id && i.status == InteractionStatus::Open)
                    .cloned()
                    .collect::<Vec<_>>(),
                r.turns_completed,
                true,
            ),
            None => (Execution::Exited, vec![], 0, false),
        });
        let hit = until.iter().find(|cond| match cond.as_str() {
            "needs_approval" => ints.iter().any(|i| {
                matches!(
                    i.kind,
                    InteractionKind::Approval | InteractionKind::PlanReview
                )
            }),
            "needs_answer" => ints.iter().any(|i| i.kind == InteractionKind::Question),
            "done" => state == Execution::Idle && turns > start_turns,
            "idle" => state == Execution::Idle && (turns > start_turns || turns_at_start.is_none()),
            "exited" => !alive || state == Execution::Exited,
            other => Execution::parse(other).is_some_and(|e| e == state),
        });
        if let Some(h) = hit {
            let r = server.with_core(|c| c.run(&run.id).map(|r| run_json(c, r)));
            return Ok(
                json!({"run": r, "state": state.as_str(), "condition": h, "interaction": ints.first()}),
            );
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(err(ErrorKind::Timeout, "condition not reached")
                .details(json!({"last_state": state.as_str()})));
        }
        let _ = tokio::time::timeout(left.min(Duration::from_millis(500)), rx.recv()).await;
    }
}

async fn resume_run(server: &Arc<Server>, run_id: &str, pane: Option<String>) -> R {
    let run: AgentRun = server
        .with_core(|c| c.store.find::<AgentRun>("run", run_id).ok().flatten())
        .ok_or_else(|| not_found("run", run_id))?;
    resume_from(server, run, pane).await
}

/// Resume a native session described by `run` (a stored run, or a template the session desk
/// builds from an indexed transcript: harness, session id, resume argv, cwd).
pub(crate) async fn resume_from(server: &Arc<Server>, run: AgentRun, pane: Option<String>) -> R {
    if headless::is_headless(&run) {
        // A new headless process continuing the session (thread/resume, --resume, session/load).
        return headless::resume(server, &run).await;
    }
    if run.resume_argv.is_empty() {
        return Err(err(ErrorKind::Unsupported, "no resume handle for this run"));
    }
    let pane = match pane {
        Some(p) => p,
        None => {
            // Prefer the pane it ran in (respawned after reboot) if it is a free shell.
            let same = server.with_core(|c| {
                c.pane(&run.pane)
                    .filter(|_| c.run_for_pane(&run.pane).is_none())
                    .map(|p| p.id.clone())
            });
            match same {
                Some(p) => p,
                None => {
                    let ws = server
                        .with_core(|c| c.model.workspaces.first().map(|w| w.id.clone()))
                        .ok_or_else(|| invalid("no workspace"))?;
                    let (_, p) = server
                        .create_tab(&ws, run.cwd.as_deref(), None, None, None)
                        .map_err(internal)?;
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    p.id
                }
            }
        }
    };
    let h = Harness::from_id(&run.harness).ok_or_else(|| invalid("unknown harness"))?;
    let line = format!("{}\r", harness::shell_join(&run.resume_argv));
    crate::render::write_and_ack(
        server,
        &pane,
        server.next_internal_input_id(),
        line.into_bytes(),
    )
    .await;
    let new = {
        let mut c = server.core.lock().unwrap();
        let mut r = new_run(&mut c, &pane, h, "process", StateSource::Process, 0.6);
        r.name = run.name.clone();
        r.harness_session_id = run.harness_session_id.clone();
        r.resume_argv = run.resume_argv.clone();
        r.task = run.task.clone();
        let mut tx = Tx::new();
        tx.counters = true;
        tx.event(
            "agent.started",
            json!({"run": r.id, "pane": pane}),
            json!({"harness": h.id(), "via": "resume", "resumed_from": run.id}),
        );
        tx.run(r.clone());
        server.commit(&mut c, tx).map_err(internal)?;
        r
    };
    Ok(json!({"run": new}))
}

pub async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    if let Some(r) = route::api(server, ctx, method, p).await {
        return Some(r);
    }
    Some(match method {
        "agent.list" => {
            let ws = s(p, "workspace")
                .map(|w| crate::api::resolve_ws(server, ctx, Some(w)).map(|w| w.id));
            let ws = match ws.transpose() {
                Ok(w) => w,
                Err(e) => return Some(Err(e)),
            };
            let harness = s(p, "harness");
            let runs: Vec<Value> = server.with_core(|c| {
                c.model
                    .runs
                    .iter()
                    .filter(|r| harness.is_none_or(|h| r.harness == h))
                    .filter(|r| {
                        ws.as_ref()
                            .is_none_or(|w| c.pane(&r.pane).is_some_and(|p| &p.workspace == w))
                    })
                    .map(|r| run_json(c, r))
                    .collect()
            });
            Ok(json!({"runs": runs}))
        }
        "agent.get" => resolve_run(server, ctx, s(p, "target")).map(|r| {
            server.with_core(|c| {
                let ints: Vec<Interaction> = c
                    .model
                    .interactions
                    .iter()
                    .filter(|i| i.run == r.id && i.status == InteractionStatus::Open)
                    .cloned()
                    .collect();
                json!({"run": run_json(c, &r), "pane": c.pane(&r.pane), "open_interactions": ints})
            })
        }),
        "agent.start" => {
            let pane = match resolve_pane(server, ctx, s(p, "pane")) {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            };
            let args: Vec<String> = p
                .get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let opts = match crate::sandbox::LaunchOpts::from_params(p) {
                Ok(o) => o,
                Err(e) => return Some(Err(e)),
            };
            start_in_pane_opts(
                server,
                &pane.id,
                s(p, "harness").unwrap_or("claude"),
                s(p, "name"),
                s(p, "prompt"),
                &args,
                None,
                &opts,
            )
            .await
        }
        "agent.spawn" => {
            let base = match resolve_pane(server, ctx, s(p, "split_of").or(s(p, "pane"))) {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            };
            let dir = vk_proto::layout::Direction::parse(s(p, "direction").unwrap_or("right"))
                .unwrap_or(vk_proto::layout::Direction::Right);
            let focus = p
                .get("focus")
                .and_then(Value::as_bool)
                .unwrap_or(false)
                .then_some(ctx.client_id.as_str());
            let pane = match server.split_pane(
                &base.id,
                dir,
                0.5,
                s(p, "cwd"),
                None,
                None,
                focus,
                &match &ctx.pane_scope {
                    Some(p) => format!("agent:{p}"),
                    None => "user".to_string(),
                },
            ) {
                Ok(p) => p,
                Err(e) => return Some(Err(internal(e))),
            };
            // Let the shell reach its prompt.
            tokio::time::sleep(Duration::from_millis(400)).await;
            let args: Vec<String> = p
                .get("args")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let opts = match crate::sandbox::LaunchOpts::from_params(p) {
                Ok(o) => o,
                Err(e) => return Some(Err(e)),
            };
            let r = start_in_pane_opts(
                server,
                &pane.id,
                s(p, "harness").unwrap_or("claude"),
                s(p, "name"),
                None,
                &args,
                None,
                &opts,
            )
            .await;
            if let (Ok(_), Some(text)) = (&r, s(p, "prompt")) {
                let mut q = json!({"target": pane.id, "text": text});
                if let Some(w) = p.get("wait") {
                    q["wait"] = w.clone();
                }
                return Some(
                    prompt(server, ctx, &q)
                        .await
                        .map(|v| json!({"pane": pane, "run": v["run"]})),
                );
            }
            r.map(|v| json!({"pane": pane, "run": v["run"]}))
        }
        "agent.prompt" => prompt(server, ctx, p).await,
        "agent.wait" => match resolve_run(server, ctx, s(p, "target")) {
            Ok(r) => {
                let until: Vec<String> = match p.get("until") {
                    Some(Value::Array(a)) => a
                        .iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect(),
                    Some(Value::String(s)) => s.split(',').map(str::to_string).collect(),
                    _ => [
                        "idle",
                        "done",
                        "needs_approval",
                        "needs_answer",
                        "error",
                        "exited",
                    ]
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                };
                wait(
                    server,
                    &r,
                    &until,
                    u(p, "timeout_ms").unwrap_or(600_000),
                    None,
                )
                .await
            }
            Err(e) => Err(e),
        },
        "agent.interrupt" => match resolve_run(server, ctx, s(p, "target")) {
            Ok(r) if headless::is_headless(&r) => {
                if headless::interrupt(server, &r) {
                    Ok(json!({"run": r}))
                } else {
                    Err(err(ErrorKind::Conflict, "the headless pane is not running"))
                }
            }
            Ok(r) => {
                crate::render::write_and_ack(
                    server,
                    &r.pane,
                    server.next_internal_input_id(),
                    b"\x1b".to_vec(),
                )
                .await;
                Ok(json!({"run": r}))
            }
            Err(e) => Err(e),
        },
        "agent.send_keys" => match resolve_run(server, ctx, s(p, "target")) {
            Ok(r) => {
                let keys = p
                    .get("keys")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let modes = crate::render::input_modes(server, &r.pane);
                let mut bytes = Vec::new();
                for k in &keys {
                    let k = k.as_str().unwrap_or_default();
                    match vk_term::keygrammar::parse_key(k) {
                        Ok(ev) => bytes.extend(vk_term::encode::encode_key(&ev, &modes)),
                        Err(e) => {
                            return Some(Err(err(ErrorKind::InvalidKey, e.to_string())
                                .details(json!({"key": k}))));
                        }
                    }
                }
                crate::render::write_and_ack(
                    server,
                    &r.pane,
                    server.next_internal_input_id(),
                    bytes,
                )
                .await;
                Ok(json!({}))
            }
            Err(e) => Err(e),
        },
        "agent.read" => match resolve_run(server, ctx, s(p, "target")) {
            Ok(r) => {
                let source = s(p, "source").unwrap_or("recent");
                if source == "transcript" {
                    let text = r
                        .transcript_path
                        .as_deref()
                        .map(|t| {
                            harness::transcript_tail(
                                std::path::Path::new(t),
                                u(p, "lines").unwrap_or(20) as usize,
                            )
                        })
                        .unwrap_or_default();
                    return Some(Ok(json!({"text": text, "source": "transcript"})));
                }
                Ok(
                    json!({"text": crate::api::read_text(server, &r.pane, source, u(p, "lines").unwrap_or(200) as usize), "source": source}),
                )
            }
            Err(e) => Err(e),
        },
        "agent.transcript" => match resolve_run(server, ctx, s(p, "target")) {
            Ok(r) => match r.transcript_path {
                Some(t) => Ok(
                    json!({"turns": harness::transcript_turns(std::path::Path::new(&t), u(p, "limit").unwrap_or(20) as usize)}),
                ),
                None => Err(err(ErrorKind::Unsupported, "no transcript for this run")
                    .details(json!({"fallback": "agent.read"}))),
            },
            Err(e) => Err(e),
        },
        "agent.rename" => match resolve_run(server, ctx, s(p, "target")) {
            Ok(r) => {
                let name = s(p, "name").map(str::to_string).filter(|n| !n.is_empty());
                if let Some(n) = &name
                    && server.with_core(|c| {
                        c.model
                            .runs
                            .iter()
                            .any(|x| x.id != r.id && x.name.as_deref() == Some(n))
                    })
                {
                    return Some(Err(err(ErrorKind::Conflict, "name_taken")));
                }
                update_run(server, &r.id, |r, tx| {
                    r.name = name.clone();
                    tx.event("agent.named", json!({"run": r.id}), json!({"name": name}));
                });
                Ok(json!({"run": server.with_core(|c| c.run(&r.id).cloned())}))
            }
            Err(e) => Err(e),
        },
        "agent.release" => match resolve_run(server, ctx, s(p, "target")) {
            Ok(r) => {
                server.agents.end_run(server, &r.id, "released");
                Ok(json!({}))
            }
            Err(e) => Err(e),
        },
        "agent.resumable" => Ok(json!({"runs": resumable_runs(server)})),
        "agent.resume" => {
            let run = match crate::api::req(p, "run") {
                Ok(r) => r.to_string(),
                Err(e) => return Some(Err(e)),
            };
            let pane = s(p, "pane").map(|x| resolve_pane(server, ctx, Some(x)).map(|p| p.id));
            let pane = match pane.transpose() {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            };
            resume_run(server, &run, pane).await
        }
        "agent.harnesses" => {
            let list: Vec<Value> = Harness::all()
                .iter()
                .map(|h| json!({"id": h.id(), "display": h.display(), "capabilities": h.capabilities(), "version_detected": harness::version(*h)}))
                .collect();
            Ok(json!({"harnesses": list}))
        }
        "agent.report" => {
            // Self-report transport (04 §4.1): agent/wrapper reports its own state.
            let pane = match resolve_pane(server, ctx, s(p, "pane")) {
                Ok(p) => p,
                Err(e) => return Some(Err(e)),
            };
            let state = s(p, "state")
                .and_then(Execution::parse)
                .ok_or_else(|| invalid("state: working|idle|error|…"));
            let state = match state {
                Ok(st) => st,
                Err(e) => return Some(Err(e)),
            };
            let h =
                Harness::from_id(s(p, "harness").unwrap_or("claude")).unwrap_or(Harness::Claude);
            let run = server.with_core(|c| c.run_for_pane(&pane.id).cloned());
            let run = match run {
                Some(r) => r,
                None => bound_run(server, &pane.id, h),
            };
            set_execution(
                server,
                &run.id,
                state,
                source_of("self_report"),
                0.9,
                s(p, "message").map(str::to_string),
            );
            Ok(json!({}))
        }

        // ---- interactions -------------------------------------------------------------------
        "interaction.list" => {
            let status = s(p, "status").unwrap_or("open");
            let mut list: Vec<Value> = server.with_core(|c| {
                let mut v: Vec<Interaction> = c
                    .model
                    .interactions
                    .iter()
                    .filter(|i| {
                        status == "all" || (status == "open" && i.status == InteractionStatus::Open)
                    })
                    .cloned()
                    .collect();
                if status != "open" {
                    v.extend(
                        c.store
                            .load_closed::<Interaction>("interaction", 100)
                            .unwrap_or_default(),
                    );
                }
                v.sort_by_key(|i| i.opened_at_ms);
                v.iter()
                    .map(|i| {
                        let mut j = serde_json::to_value(i).unwrap_or(Value::Null);
                        j["kind"] = json!(i.kind.as_str());
                        j["pane_handle"] = json!(c.pane(&i.pane).map(|p| p.handle.clone()));
                        j
                    })
                    .collect()
            });
            if let Some(k) = s(p, "kind") {
                list.retain(|i| i["kind"] == k);
            }
            Ok(json!({"interactions": list}))
        }
        "interaction.get" => {
            let id = s(p, "interaction").unwrap_or("");
            server
                .with_core(|c| {
                    c.interaction(id).cloned().or_else(|| {
                        c.store
                            .find::<Interaction>("interaction", id)
                            .ok()
                            .flatten()
                    })
                })
                .map(|i| json!({"interaction": i}))
                .ok_or_else(|| not_found("interaction", id))
        }
        "interaction.answer" => answer(server, ctx, p).await,
        "interaction.cancel" => {
            let id = s(p, "interaction").unwrap_or("").to_string();
            match server.with_core(|c| c.interaction(&id).map(|i| i.id.clone())) {
                Some(id) => {
                    resolve(server, &id, InteractionStatus::Cancelled, "dismissed");
                    Ok(json!({}))
                }
                None => Err(not_found("interaction", &id)),
            }
        }

        // ---- adapter transport ------------------------------------------------------------
        "adapter.signal" | "adapter.gate" => {
            let Some(pane) = ctx.pane_scope.clone() else {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "adapter methods need a pane token",
                )));
            };
            let Some(h) = s(p, "harness").and_then(Harness::from_id) else {
                return Some(Ok(json!({})));
            };
            let h = route::effective(server, &pane, h);
            let event = s(p, "event").unwrap_or("").to_string();
            let payload = p.get("payload").cloned().unwrap_or(Value::Null);
            if method == "adapter.signal" {
                route::signal(server, &pane, h, &event, &payload);
                Ok(json!({}))
            } else {
                gate(server, &pane, h, &event, &payload).await
            }
        }
        "adapter.delivery_ack" => {
            // Only the hook shim of the interaction's own pane, with the issued key, may ack.
            let Some(pane) = ctx.pane_scope.clone() else {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "adapter methods need a pane token",
                )));
            };
            let it =
                s(p, "interaction").and_then(|id| server.with_core(|c| c.interaction(id).cloned()));
            let key_ok = it.as_ref().is_some_and(|i| {
                s(p, "idempotency_key") == Some(&format!("{}:{}", i.id, i.decision_rev))
            });
            if it.as_ref().is_none_or(|i| i.pane != pane) || !key_ok {
                return Some(Err(err(
                    ErrorKind::PermissionDenied,
                    "delivery ack does not match an interaction of this pane",
                )));
            }
            if let Some(id) = s(p, "interaction") {
                // Claude/Codex: the shim printed the decision; Post*/resolution events confirm.
                set_delivery(server, id, DeliveryState::Delivered, None);
            }
            Ok(json!({}))
        }
        _ => return None,
    })
}

#[allow(dead_code)]
fn subject(p: &Pane) -> Value {
    subject_pane(p)
}

#[cfg(test)]
pub(crate) fn harness_tests_blank() -> Interaction {
    harness::interaction_from_hook(
        Harness::Claude,
        "PermissionRequest",
        &json!({"tool_name": "Bash", "tool_input": {"command": "x"}}),
    )
    .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIALOG: &str = "│ Bash command\n│   rm -rf build\n│ Do you want to proceed?\n│ ❯ 1. Yes\n│   2. Yes, and don't ask again\n│   3. No, and tell Claude what to do differently (esc)";

    #[test]
    fn stale_answer_never_matches_a_replacement_dialog() {
        let d = screen::evaluate(Harness::Claude, DIALOG).dialog.unwrap();
        let mine = harness::interaction_from_hook(
            Harness::Claude,
            "PermissionRequest",
            &json!({"tool_name": "Bash", "tool_input": {"command": "rm -rf build"}}),
        )
        .unwrap();
        assert!(dialog_matches(&d, &mine));
        let other = harness::interaction_from_hook(
            Harness::Claude,
            "PermissionRequest",
            &json!({"tool_name": "Bash", "tool_input": {"command": "ls"}}),
        )
        .unwrap();
        assert!(
            !dialog_matches(&d, &other),
            "a harmless approval must not select options of a dangerous dialog"
        );
        let mut screen_it = mine.clone();
        screen_it.native_ref = Some(format!("screen:{}", d.fingerprint));
        assert!(dialog_matches(&d, &screen_it));
        screen_it.native_ref = Some("screen:deadbeef".into());
        assert!(!dialog_matches(&d, &screen_it));
    }
}
