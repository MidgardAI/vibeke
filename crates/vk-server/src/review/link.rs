//! Lane 2C, spec 15 §3 / §6.2 / §10.3: the **Link run** step.
//!
//! Tracking and binding refuse a run whose identity is only suggested (`binding_unverified`).
//! `task.link.status {run | pane}` explains why a run is not verified and what makes it so,
//! and lists the *verified* runs the user can link instead (same pane first, then the same
//! workspace), so a client can offer an explicit choice. It never binds anything and never
//! edits harness configuration: installing an integration stays the explicit setup flow.

use super::*;

fn reasons_and_remedies(run: &AgentRun) -> (Vec<String>, Vec<Value>) {
    let mut reasons = vec![];
    if run.harness_session_id.is_none() {
        reasons.push(
            "The harness has not reported its session id (no structured integration event yet)."
                .to_string(),
        );
    }
    match run.integration.as_str() {
        "" | "process" => reasons.push(
            "Detected from the process only: no tested integration is reporting for this agent."
                .into(),
        ),
        "screen" => reasons.push(
            "Observed from the screen only: screen state is a guess, not an identity.".into(),
        ),
        "self_report" => reasons.push(
            "Self-reported state: an agent's own report can't establish its identity.".into(),
        ),
        _ => {}
    }
    let remedies = vec![
        json!({
            "action": "install_integration",
            "label": "Install the integration, then restart or resume the agent",
            "command": format!("vibeke setup   # or: vibeke integration install {}", run.harness),
            "note": "Installing edits the harness's own configuration; it is never done implicitly.",
        }),
        json!({
            "action": "start_in_vibeke",
            "label": "Start the agent through Vibeke (its identity is then known from the launch)",
            "command": format!("vibeke agent start {}", run.harness),
        }),
        json!({
            "action": "link_verified_run",
            "label": "Link a verified run listed below instead",
        }),
    ];
    (reasons, remedies)
}

/// `task.link.status {run | pane}`: identity status of a run, why it is not verified, remedies
/// and the verified runs nearby. Read-only.
pub(super) fn status_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = tracking::find_run(server, ctx, p)?;
    let scope = caller_workspace(server, ctx)?;
    if let Some(ws) = &scope
        && server
            .with_core(|c| c.pane(&run.pane).map(|x| x.workspace.clone()))
            .as_deref()
            != Some(ws.as_str())
    {
        return Err(not_found("run", &run.id));
    }
    let verified = tracking::identity(&run).deterministic;
    let (reasons, remedies) = if verified {
        (vec![], vec![])
    } else {
        reasons_and_remedies(&run)
    };
    let candidates: Vec<Value> = server.with_core(|c| {
        let ws = c.pane(&run.pane).map(|x| x.workspace.clone());
        let mut v: Vec<(u8, i64, Value)> = c
            .model
            .runs
            .iter()
            .filter(|r| r.id != run.id && r.ended_at_ms.is_none())
            .filter(|r| tracking::identity(r).deterministic)
            .filter_map(|r| {
                let rws = c.pane(&r.pane).map(|x| x.workspace.clone());
                if scope.as_deref().is_some_and(|s| rws.as_deref() != Some(s)) {
                    return None;
                }
                let rank = if r.pane == run.pane {
                    0
                } else if rws.is_some() && rws == ws {
                    1
                } else {
                    return None;
                };
                Some((
                    rank,
                    -r.started_at_ms,
                    json!({
                        "run": r.id,
                        "handle": r.handle,
                        "harness": r.harness,
                        "name": r.name,
                        "pane": r.pane,
                        "integration": r.integration,
                        "turns": r.turns_completed,
                        "same_pane": r.pane == run.pane,
                        "started_at_ms": r.started_at_ms,
                    }),
                ))
            })
            .collect();
        v.sort_by_key(|a| (a.0, a.1));
        v.into_iter().map(|(_, _, j)| j).collect()
    });
    Ok(json!({
        "run": {
            "id": run.id,
            "handle": run.handle,
            "harness": run.harness,
            "integration": run.integration,
            "pane": run.pane,
            "session_reported": run.harness_session_id.is_some(),
        },
        "verified": verified,
        "reasons": reasons,
        "remedies": remedies,
        "candidates": candidates,
        "note": "Linking is explicit: choose a verified run to track; nothing is bound or installed by this call.",
    }))
}
