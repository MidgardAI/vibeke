//! Signal routing for every harness family (04 §3.1 AdapterHost): hooks (Claude, Codex,
//! Gemini), extensions (pi/omp, OpenCode), the ACP host and self-report all end up in the same
//! hook vocabulary (`on_signal`) so state, interactions and tracking (15 T1) work identically.
//! Also the API entry point for methods added in M2 (ACP launch, Herdr self-report, manifests).

use super::harness::{Family, Harness};
use super::*;

/// The run's own harness when the signal comes from its family: a custom wrapper (`espi`)
/// whose pi extension reports `harness: "pi"` keeps its run instead of being replaced.
pub(super) fn effective(server: &Server, pane: &str, h: Harness) -> Harness {
    let Some(run) = server.with_core(|c| c.run_for_pane(pane).map(|r| r.harness.clone())) else {
        return h;
    };
    if run == h.id() {
        return h;
    }
    match Harness::from_id(&run) {
        Some(rh) if rh.base() == h.base() && rh != h => rh,
        _ => h,
    }
}

/// `adapter.signal` for any harness family.
pub(super) fn signal(server: &Arc<Server>, pane: &str, h: Harness, event: &str, p: &Value) {
    match h.family() {
        Family::Pi | Family::Omp => on_extension_signal(server, pane, h, event, p),
        Family::OpenCode => super::opencode::on_event(server, pane, h, event, p),
        Family::Gemini => super::gemini::on_hook(server, pane, h, event, p),
        Family::Acp => super::acp::on_signal(server, pane, h, event, p),
        Family::Claude | Family::Codex | Family::Generic => on_signal(server, pane, h, event, p),
    }
}

/// An approval the harness shows in its own UI and Vibeke can only observe (Gemini tool
/// confirmation, Herdr `blocked`): answerable by best-effort keystrokes when `answerable`.
#[allow(clippy::too_many_arguments)]
pub(super) fn open_observed(
    server: &Arc<Server>,
    run: &AgentRun,
    native_ref: Option<String>,
    tool: &str,
    input: Value,
    source: StateSource,
    confidence: f32,
    answerable: bool,
) -> Option<Interaction> {
    if native_ref.is_some()
        && server.with_core(|c| {
            c.model.interactions.iter().any(|i| {
                i.run == run.id && i.native_ref == native_ref && i.status == InteractionStatus::Open
            })
        })
    {
        return None;
    }
    let mut it = harness::interaction_from_hook(
        Harness::Claude,
        "PermissionRequest",
        &json!({"tool_name": tool, "tool_input": input}),
    )?;
    let mut c = server.core.lock().unwrap();
    it.handle = c.next_interaction_handle();
    it.run = run.id.clone();
    it.pane = run.pane.clone();
    it.native_ref = native_ref;
    it.source = source;
    it.confidence = confidence;
    it.answerable = answerable;
    it.answer_channel = if answerable {
        AnswerChannel::Keystrokes
    } else {
        AnswerChannel::None
    };
    let src = match source {
        StateSource::SelfReport => "self_report",
        StateSource::Screen => "screen",
        _ => "structured",
    };
    let mut tx = Tx::new();
    tx.counters = true;
    tx.event(
        "interaction.opened",
        json!({"interaction": it.id, "pane": run.pane, "run": run.id}),
        json!({"kind": it.kind.as_str(), "source": src, "confidence": confidence, "harness": run.harness, "answerable": answerable}),
    );
    tx.interaction(it.clone());
    let _ = server.commit(&mut c, tx);
    drop(c);
    notify_interaction(server, &it, run);
    Some(it)
}

/// Open interactions of a run, optionally only those from `source`.
pub(super) fn open_of(server: &Server, run: &str, source: Option<StateSource>) -> Vec<String> {
    server.with_core(|c| {
        c.model
            .interactions
            .iter()
            .filter(|i| {
                i.run == run
                    && i.status == InteractionStatus::Open
                    && source.is_none_or(|s| i.source == s)
            })
            .map(|i| i.id.clone())
            .collect()
    })
}

pub(super) fn file_changed(server: &Server, run: &AgentRun, path: &str, op: &str) {
    update_run(server, &run.id, |r, tx| {
        tx.event(
            "agent.file_changed",
            json!({"run": r.id, "pane": r.pane}),
            json!({"path": path, "op": op}),
        );
    });
}

/// M2 API methods (04 §4.1, §6.6, 07 §8): returns `None` for methods handled elsewhere.
pub(super) async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    match method {
        "pane.report_agent" | "pane.report_agent_session" | "adapter.report_self" => {
            Some(super::selfreport::api(server, ctx, method, p))
        }
        // Headless (01 §3.3): the harness runs under a pipe-mode holder in a new tab.
        "agent.start" | "agent.spawn" if s(p, "mode") == Some("headless") => {
            Some(super::headless::start(server, Some(ctx), p).await)
        }
        "agent.start" | "agent.spawn" if s(p, "acp").is_some() => {
            Some(super::acp::start(server, ctx, method, p).await)
        }
        "agent.manifests" => Some(Ok(manifests_json())),
        "agent.manifests_reload" => {
            let warnings = super::manifests::reload();
            Some(Ok(
                json!({"warnings": warnings, "manifests": manifests_json()["manifests"]}),
            ))
        }
        _ => None,
    }
}

fn manifests_json() -> Value {
    let list: Vec<Value> = super::manifests::all()
        .iter()
        .filter(|(_, l)| super::manifests::repo_active(l))
        .map(|(_, l)| {
            json!({
                "id": l.m.id,
                "name": l.display(),
                "source": l.source.label(),
                "family": l.family,
                "transports": l.m.integration.transports,
                "validated_range": l.validated_range(),
                "capabilities_unversioned": l.capabilities(None, "tui"),
                "capabilities_unverified": l.unverified_capabilities(),
                "detects": !l.m.detect.process.is_empty(),
                "screen_rules": l.has_screen_rules(),
                "warnings": l.warnings,
            })
        })
        .collect();
    json!({"manifests": list, "warnings": super::manifests::warnings()})
}
