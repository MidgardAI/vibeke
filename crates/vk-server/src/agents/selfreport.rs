//! Self-report transport (04 §4.1): `adapter.report_self` and the Herdr-compatible
//! `pane.report_agent` / `pane.report_agent_session` with Herdr's parameter names, accepted on
//! the main socket so existing Herdr integrations can report into Vibeke when pointed at it
//! (`vibeke api call pane.report_agent …`). The outer `HERDR_*` env stays stripped from panes
//!; the Herdr-compat socket and `HERDR_SOCKET_PATH` are M5.
//!
//! Rules (04 §4.1, §2.5): reports with `seq` ≤ the last seen from the same `(pane, source)` are
//! dropped (Herdr's rule); self-report never overrides a live structured transport (rule 5:
//! structured > self_report); Herdr's `blocked` opens a provisional approval (confidence 0.8,
//! source `self_report`) that only best-effort keystrokes can answer, and is closed by the next
//! `working`/`idle` report from that source.

use super::harness::Harness;
use super::*;
use std::sync::LazyLock;

static SEQ: LazyLock<Mutex<HashMap<(String, String), i64>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// `true` when the report is fresh (strictly greater `seq` than the last one from `source`).
pub(super) fn accept_seq(pane: &str, source: &str, seq: Option<i64>) -> bool {
    let Some(seq) = seq else { return true };
    let mut g = SEQ.lock().unwrap();
    let k = (pane.to_string(), source.to_string());
    match g.get(&k) {
        Some(last) if seq <= *last => false,
        _ => {
            g.insert(k, seq);
            true
        }
    }
}

fn seq_of(p: &Value) -> Option<i64> {
    p.get("seq").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_u64().map(|u| u as i64))
            .or_else(|| v.as_f64().map(|f| f as i64))
    })
}

/// Herdr agent names → Vibeke harness ids; unknown agents use the generic screen manifest.
fn harness_for(agent: Option<&str>) -> Harness {
    let id = match agent.unwrap_or("") {
        "claude-code" | "claude_code" => "claude",
        "oh-my-pi" => "omp",
        "gemini-cli" => "gemini",
        a => a,
    };
    Harness::from_id(id)
        .or_else(|| Harness::from_id("generic-repl"))
        .unwrap_or(Harness::Claude)
}

fn target_pane(server: &Server, ctx: &Ctx, p: &Value) -> Result<String, vk_proto::rpc::RpcError> {
    let want = s(p, "pane_id").or(s(p, "pane"));
    match &ctx.pane_scope {
        // A pane may only report for itself (or panes it created), like every adapter call.
        Some(scope) => {
            let Some(w) = want else {
                return Ok(scope.clone());
            };
            let pane = resolve_pane(server, ctx, Some(w))?;
            if &pane.id == scope || pane.created_by == format!("agent:{scope}") {
                Ok(pane.id)
            } else {
                Err(err(
                    ErrorKind::PermissionDenied,
                    "self-report is limited to your own pane",
                ))
            }
        }
        None => Ok(resolve_pane(server, ctx, want)?.id),
    }
}

/// The pane's run, or a new self-report run for `h`.
fn run_for(server: &Arc<Server>, pane: &str, h: Harness) -> AgentRun {
    if let Some(r) = server.with_core(|c| c.run_for_pane(pane).cloned()) {
        return r;
    }
    let mut c = server.core.lock().unwrap();
    let run = new_run(&mut c, pane, h, "self_report", StateSource::SelfReport, 0.9);
    let mut tx = Tx::new();
    tx.counters = true;
    tx.event(
        "agent.started",
        json!({"run": run.id, "pane": pane}),
        json!({"harness": h.id(), "via": "self_report"}),
    );
    tx.run(run.clone());
    let _ = server.commit(&mut c, tx);
    run
}

/// A structured transport that still drives the run outranks self-report (04 §2.5 rule 5).
fn structured_live(run: &AgentRun) -> bool {
    run.execution.source == StateSource::Structured
        && run.health == AdapterHealth::Healthy
        && run.integration != "self_report"
        && run.integration != "process"
}

pub(super) fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> R {
    let pane = target_pane(server, ctx, p)?;
    let source = s(p, "source").unwrap_or(match method {
        "adapter.report_self" => "vibeke",
        _ => "herdr",
    });
    if !accept_seq(&pane, source, seq_of(p)) {
        return Ok(json!({"type": "ok", "dropped": "stale_seq"}));
    }
    let h = harness_for(s(p, "agent").or(s(p, "harness")));
    let run = run_for(server, &pane, h);
    match method {
        "pane.report_agent_session" => {
            let sid = s(p, "agent_session_id").map(str::to_string);
            let path = s(p, "agent_session_path").map(str::to_string);
            let rh = Harness::from_id(&run.harness).unwrap_or(h);
            update_run(server, &run.id, |r, tx| {
                if let Some(sid) = &sid
                    && r.harness_session_id.as_deref() != Some(sid.as_str())
                {
                    r.harness_session_id = Some(sid.clone());
                    if r.resume_argv.is_empty() || r.integration == "self_report" {
                        r.resume_argv = rh.resume_argv(sid);
                    }
                    tx.event(
                        "agent.identified",
                        json!({"run": r.id, "pane": r.pane}),
                        json!({"harness_session_id": sid, "transcript_path": path, "source": "self_report", "session_start_source": s(p, "session_start_source")}),
                    );
                }
                if path.is_some() {
                    r.transcript_path = path.clone();
                }
            });
            Ok(json!({"type": "ok"}))
        }
        _ => {
            let state = s(p, "state").unwrap_or("");
            if let Some(argv) = p.get("resume_argv").and_then(Value::as_array) {
                let argv: Vec<String> = argv
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect();
                update_run(server, &run.id, |r, _| r.resume_argv = argv);
            }
            // Any non-blocked report closes this source's provisional interaction.
            if state != "blocked" {
                for id in super::route::open_of(server, &run.id, Some(StateSource::SelfReport)) {
                    resolve(
                        server,
                        &id,
                        InteractionStatus::ResolvedElsewhere,
                        "self-report moved on",
                    );
                }
            }
            if state == "blocked" {
                let msg = s(p, "message").unwrap_or("agent is blocked");
                let answerable =
                    super::screen::screen_manifest(Harness::from_id(&run.harness).unwrap_or(h))
                        .is_some_and(|l| l.has_dialog_rules())
                        || matches!(
                            Harness::from_id(&run.harness).map(|x| x.base()),
                            Some(Harness::Claude | Harness::Codex | Harness::Pi | Harness::Omp)
                        );
                super::route::open_observed(
                    server,
                    &run,
                    Some(format!("self_report:{source}")),
                    "agent",
                    json!({"command": msg}),
                    StateSource::SelfReport,
                    0.8,
                    answerable,
                );
                return Ok(json!({"type": "ok"}));
            }
            let exec = match state {
                // Herdr spells "done" for idle-after-work in some integrations.
                "done" => Some(Execution::Idle),
                other => Execution::parse(other),
            };
            let Some(exec) = exec else {
                return Err(invalid(format!(
                    "state: idle|working|blocked (or a Vibeke execution state), got {state:?}"
                )));
            };
            let run = server.with_core(|c| c.run(&run.id).cloned()).unwrap_or(run);
            if structured_live(&run) {
                // Information only: never overwrite structured state.
                if run.execution.value != exec {
                    super::arbiter::disagreement(
                        server,
                        &run,
                        "execution",
                        run.execution.value.as_str(),
                        exec.as_str(),
                        "self_report",
                    );
                }
                return Ok(json!({"type": "ok", "applied": false}));
            }
            set_execution(
                server,
                &run.id,
                exec,
                StateSource::SelfReport,
                0.9,
                s(p, "message").map(str::to_string),
            );
            Ok(json!({"type": "ok", "applied": true}))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seq_drop_rule_is_per_source() {
        assert!(accept_seq("p-sr", "herdr", Some(5)));
        assert!(!accept_seq("p-sr", "herdr", Some(5)));
        assert!(!accept_seq("p-sr", "herdr", Some(4)));
        assert!(accept_seq("p-sr", "other", Some(1)));
        assert!(accept_seq("p-sr", "herdr", Some(6)));
        assert!(accept_seq("p-sr", "herdr", None));
        assert_eq!(seq_of(&json!({"seq": 1.7e15})), Some(1_700_000_000_000_000));
    }

    #[test]
    fn herdr_agent_names() {
        assert_eq!(harness_for(Some("claude")).id(), "claude");
        assert_eq!(harness_for(Some("gemini-cli")).id(), "gemini");
        assert_eq!(harness_for(Some("hermes")).id(), "hermes");
        assert_eq!(harness_for(Some("mystery-bot")).id(), "generic-repl");
    }
}
