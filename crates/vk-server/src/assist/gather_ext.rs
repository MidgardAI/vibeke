//! Context gathering for the A2/A3 operations (semantic navigation, decision cards, stall
//! notices, task titles) and for sources collected from other machines (14 §4, §7.1).
//!
//! Everything here reads Vibeke's own state through the same accessors the other operations
//! use, only after the caller's consent was checked for every workspace involved, and hands
//! the result over as bounded `SourceInput`s: redaction, fencing and limits happen once, in
//! the context package. Nothing here calls a mutating method.

use super::*;
use std::collections::HashMap;
use vk_assist::consent::Grant;
use vk_assist::navigate::{self, Candidate};
use vk_assist::remote::{self, Cursors, Freshness};
use vk_assist::stall;

/// Candidates offered to the model for one navigation query (the rest are reported omitted).
const NAV_LIMIT: usize = 40;

/// Sources for `reply_suggestions`: the pane's metadata, the agent's recent requests and last
/// message, its open interaction (if any) and, when selected, a screen excerpt.
pub(super) async fn reply_sources(
    server: &Arc<Server>,
    ctx: &Ctx,
    t: &Target,
    sources: &mut Vec<SourceInput>,
) -> Result<(), RpcError> {
    if let Some(pane) = &t.pane {
        let mut meta = format!(
            "title: {}\ncwd: {}",
            pane.title.as_deref().unwrap_or(&pane.auto_title),
            pane.cwd.as_deref().unwrap_or("?"),
        );
        if let Some(run) = &t.run {
            meta.push_str(&format!("\nagent: {}", run.harness));
        }
        sources.push(src(
            "pane",
            json!({"pane": pane.id}),
            "pane metadata",
            meta,
            None,
        ));
    }
    if let Some(run) = &t.run {
        sources.extend(turn_sources(server, run, &t.turns, 3));
        if let Some(m) = &run.last_message {
            sources.push(src(
                "agent_message",
                json!({"run": run.id, "field": "last_message"}),
                "agent's last message (claim)",
                clip(m, 2000).0,
                None,
            ));
        }
        let open = server.with_core(|c| {
            c.model
                .interactions
                .iter()
                .find(|i| i.run == run.id && i.status == InteractionStatus::Open)
                .cloned()
        });
        if let Some(i) = open {
            let mut text = format!("kind: {:?}\ntitle: {}\n", i.kind, i.title);
            if let Some(b) = &i.body_md {
                text.push_str(&format!("body: {}\n", clip(b, 800).0));
            }
            sources.push(src(
                "interaction",
                json!({"interaction": i.id, "run": i.run, "decision_rev": i.decision_rev}),
                format!("open interaction {} ({:?})", i.handle, i.kind),
                text,
                Some(i.opened_at_ms),
            ));
        }
    }
    if t.include_screen
        && let Some(pane) = &t.pane
    {
        let r = read_call(
            server,
            ctx,
            "pane.read",
            json!({"pane": pane.id, "lines": 40}),
        )
        .await?;
        sources.push(src(
            "screen",
            json!({"pane": pane.id, "revision": r["revision"]}),
            "screen excerpt (inferred, last 40 lines)",
            r["text"].as_str().unwrap_or(""),
            Some(now()),
        ));
    }
    if sources.is_empty() {
        return Err(invalid("nothing to base a reply on"));
    }
    Ok(())
}

/// Gather the sources of one of the new operations. Returns the kind of each target ID
/// (Vibeke's own record, never the model's).
pub(super) async fn gather(
    server: &Arc<Server>,
    ctx: &Ctx,
    op: Operation,
    t: &Target,
    sources: &mut Vec<SourceInput>,
    targets: &mut Vec<String>,
) -> Result<HashMap<String, String>, RpcError> {
    let mut kinds: HashMap<String, String> = HashMap::new();
    match op {
        Operation::Navigate => {
            let query = t.query.as_deref().unwrap_or("");
            let cands = candidates(server, t);
            if cands.is_empty() {
                return Err(invalid("nothing in this workspace to search"));
            }
            let ranked = navigate::rank(query, cands, NAV_LIMIT);
            sources.push(src(
                "query",
                json!({"query": true}),
                "the user's query",
                query,
                Some(now()),
            ));
            for c in &ranked.candidates {
                targets.push(c.id.clone());
                kinds.insert(c.id.clone(), c.kind.clone());
                sources.push(src(
                    "candidate",
                    json!({"candidate": c.id, "kind": c.kind}),
                    c.label.clone(),
                    c.text.clone(),
                    None,
                ));
            }
            if ranked.omitted > 0 || ranked.unranked {
                sources.push(src(
                    "coverage",
                    json!({"coverage": true}),
                    "coverage",
                    format!(
                        "{} further candidate(s) were not sent (ranked lower).{}",
                        ranked.omitted,
                        if ranked.unranked {
                            " No candidate shared a term with the query; the list is ordered by recency only."
                        } else {
                            ""
                        }
                    ),
                    None,
                ));
            }
        }
        Operation::DecisionCard => {
            let i = t.interaction.as_ref().expect("checked");
            if i.status != InteractionStatus::Open {
                return Err(err(
                    ErrorKind::Conflict,
                    "interaction_not_open: decision cards explain open interactions only",
                )
                .details(json!({"reason": "interaction_not_open", "status": i.status})));
            }
            targets.push(i.id.clone());
            kinds.insert(i.id.clone(), "interaction".into());
            let mut text = format!(
                "id: {}\nkind: {:?}\ntitle: {}\nstatus: open (live at generation)\nopened_at_ms: {}\n",
                i.id, i.kind, i.title, i.opened_at_ms
            );
            if let Some(b) = &i.body_md {
                text.push_str(&format!("body: {}\n", clip(b, 1500).0));
            }
            if let Some(a) = &i.action {
                text.push_str(&format!(
                    "action tool: {}\nsummary: {}\ncommand: {}\npaths: {}\nrisk: {:?} {}\n",
                    a.tool,
                    clip(&a.summary, 300).0,
                    a.command
                        .as_deref()
                        .map(|c| clip(c, 600).0)
                        .unwrap_or_default(),
                    a.paths
                        .iter()
                        .take(10)
                        .cloned()
                        .collect::<Vec<_>>()
                        .join(", "),
                    a.risk,
                    a.risk_reasons.join("; ")
                ));
            }
            if let Some(p) = &i.plan_md {
                text.push_str(&format!("plan: {}\n", clip(p, 1000).0));
            }
            if !i.questions.is_empty() {
                text.push_str(&format!(
                    "questions: {}\n",
                    clip(
                        &serde_json::to_string(&i.questions).unwrap_or_default(),
                        1500
                    )
                    .0
                ));
            }
            sources.push(src(
                "interaction",
                json!({"interaction": i.id, "run": i.run, "decision_rev": i.decision_rev}),
                format!("open interaction {} ({:?})", i.handle, i.kind),
                text,
                Some(i.opened_at_ms),
            ));
            if let Some(run) = &t.run {
                sources.extend(turn_sources(server, run, &[], 3));
                if let Some(m) = &run.last_message {
                    sources.push(src(
                        "agent_message",
                        json!({"run": run.id, "field": "last_message"}),
                        "agent's last message (claim)",
                        clip(m, 1500).0,
                        None,
                    ));
                }
            }
            let earlier: Vec<Interaction> = server.with_core(|c| {
                let v: Vec<Interaction> = c
                    .model
                    .interactions
                    .iter()
                    .filter(|x| {
                        x.run == i.run && x.id != i.id && x.status != InteractionStatus::Open
                    })
                    .cloned()
                    .collect();
                v.into_iter().rev().take(5).collect()
            });
            for x in earlier.into_iter().rev() {
                let answer = x
                    .answer
                    .as_ref()
                    .map(|a| clip(&serde_json::to_string(a).unwrap_or_default(), 300).0)
                    .unwrap_or_else(|| "-".into());
                sources.push(src(
                    "earlier_decision",
                    json!({"interaction": x.id, "run": x.run}),
                    format!("earlier {} — {}", x.handle, clip(&x.title, 80).0),
                    format!(
                        "title: {}\nstatus: {:?}\nanswer: {answer}",
                        x.title, x.status
                    ),
                    x.answered_at_ms.or(Some(x.opened_at_ms)),
                ));
            }
        }
        Operation::StallNotice => {
            let run = t.run.as_ref().expect("checked");
            let (cfg, _) = config()?;
            let obs: Vec<stall::Obs> = crate::tracking::items_of(server, &run.id)
                .into_iter()
                .filter_map(|r| {
                    r.command.map(|command| stall::Obs {
                        command,
                        exit_code: r.exit_code,
                        ended_at_ms: r.ended_at_ms,
                    })
                })
                .collect();
            let Some(sig) = stall::detect(&obs, cfg.stall_repeat_threshold) else {
                return Err(err(
                    ErrorKind::Conflict,
                    "no_signal: the run shows no repeated failing command, so there is nothing to check",
                )
                .details(json!({"reason": "no_signal"})));
            };
            targets.push(run.id.clone());
            kinds.insert(run.id.clone(), "run".into());
            sources.push(src(
                "stall_signal",
                json!({"run": run.id, "signal": sig.digest()}),
                "repetition signal (recorded commands)",
                sig.render(),
                sig.last_ms,
            ));
            sources.extend(turn_sources(server, run, &[], 2));
            if let Some(m) = &run.last_message {
                sources.push(src(
                    "agent_message",
                    json!({"run": run.id, "field": "last_message"}),
                    "agent's last message (claim)",
                    clip(m, 1000).0,
                    None,
                ));
            }
        }
        Operation::TaskTitle => {
            if let Some(task) = &t.task {
                kinds.insert(task.id.clone(), "task".into());
                targets.push(task.id.clone());
                sources.push(src(
                    "task",
                    json!({"task": task.id}),
                    format!("task {}", task.handle),
                    format!("current title: {}\nstatus: {}", task.title, task.status),
                    Some(task.created_at_ms),
                ));
                if let Ok(i) =
                    read_call(server, ctx, "task.intent.get", json!({"task": task.id})).await
                    && !i["intent"].is_null()
                {
                    sources.push(src(
                        "intent",
                        json!({"task": task.id, "revision": i["intent"]["revision"]}),
                        format!("confirmed intent of {}", task.handle),
                        clip(
                            &serde_json::to_string_pretty(&i["intent"]).unwrap_or_default(),
                            4000,
                        )
                        .0,
                        None,
                    ));
                }
            }
            let run = t.run.clone().or_else(|| {
                t.task.as_ref().and_then(|task| {
                    crate::tracking::task_bindings(server, &task.id)
                        .last()
                        .and_then(|b| server.with_core(|c| c.run(&b.run_id).cloned()))
                })
            });
            if let Some(run) = run {
                sources.extend(turn_sources(server, &run, &t.turns, 1));
            }
            if sources.is_empty() {
                return Err(invalid("no task or recorded request to title"));
            }
        }
        Operation::ReplySuggestions => {
            reply_sources(server, ctx, t, sources).await?;
        }
        // Handled with the briefing in the caller.
        Operation::BackgroundSummary => {}
        _ => {}
    }
    Ok(kinds)
}

/// Navigation candidates of the target's workspace (only that workspace: its consent was
/// checked).
pub(super) fn candidates(server: &Arc<Server>, t: &Target) -> Vec<Candidate> {
    let (runs, tasks, inters, panes) = server.with_core(|c| {
        let panes: Vec<Pane> = c
            .model
            .panes
            .iter()
            .filter(|p| p.workspace == t.ws.id)
            .cloned()
            .collect();
        let ids: Vec<&str> = panes.iter().map(|p| p.id.as_str()).collect();
        let runs: Vec<AgentRun> = c
            .model
            .runs
            .iter()
            .filter(|r| ids.contains(&r.pane.as_str()) && r.ended_at_ms.is_none())
            .cloned()
            .collect();
        let inters: Vec<Interaction> = c
            .model
            .interactions
            .iter()
            .filter(|i| ids.contains(&i.pane.as_str()) && i.status == InteractionStatus::Open)
            .cloned()
            .collect();
        let tasks: Vec<Task> = c
            .model
            .tasks
            .iter()
            .filter(|x| x.workspace.as_deref() == Some(t.ws.id.as_str()))
            .cloned()
            .collect();
        (runs, tasks, inters, panes)
    });
    let mut out = vec![];
    let mut covered: Vec<&str> = vec![];
    for r in &runs {
        covered.push(r.pane.as_str());
        let last_request = crate::tracking::turns_of(server, &r.id, 1)
            .into_iter()
            .next()
            .map(|t| clip(&t.prompt, 240).0)
            .unwrap_or_default();
        out.push(Candidate {
            id: r.id.clone(),
            kind: "run".into(),
            label: format!("{} ({})", r.name.as_deref().unwrap_or(&r.handle), r.harness),
            text: format!(
                "state {:?}; cwd {}; last tool {}; latest request: {}",
                r.execution.value,
                r.cwd.as_deref().unwrap_or("?"),
                r.last_tool.as_deref().unwrap_or("-"),
                last_request
            ),
            recency: r.started_at_ms,
        });
    }
    for x in &tasks {
        out.push(Candidate {
            id: x.id.clone(),
            kind: "task".into(),
            label: x.title.clone(),
            text: format!(
                "task {}; status {}; branch {}",
                x.handle,
                x.status,
                x.branch.as_deref().unwrap_or("-")
            ),
            recency: x.created_at_ms,
        });
    }
    for i in &inters {
        out.push(Candidate {
            id: i.id.clone(),
            kind: "interaction".into(),
            label: i.title.clone(),
            text: format!(
                "open {:?}: {}",
                i.kind,
                clip(i.body_md.as_deref().unwrap_or(""), 200).0
            ),
            recency: i.opened_at_ms,
        });
    }
    for p in &panes {
        if covered.contains(&p.id.as_str()) {
            continue;
        }
        out.push(Candidate {
            id: p.id.clone(),
            kind: "pane".into(),
            label: p.display_title().to_string(),
            text: format!(
                "cwd {}; running {}",
                p.cwd.as_deref().unwrap_or("?"),
                p.fg_cmdline.join(" ")
            ),
            recency: 0,
        });
    }
    out
}

// ---- sources from other machines ----------------------------------------------------------------------

pub(super) struct RemotePrep {
    pub inputs: Vec<SourceInput>,
    /// Coverage notes: shown in the preview and passed to the model.
    pub notes: Vec<String>,
    /// Consent identities (`machine:path`) of every included remote workspace.
    pub identities: Vec<String>,
    pub cursors: Cursors,
}

/// Validate `remote_sources` (collected by a client from other machines) and turn them into
/// sources plus coverage notes. Each included remote workspace needs its own consent; an
/// offline one is reported, never silently dropped; stale and gapped ones are labelled.
pub(super) fn remote_prepare(
    server: &Server,
    cfg: &AssistConfig,
    p: &Value,
    resolved: &Resolved,
    op: Operation,
    classes: &[&str],
    grants: &[Grant],
) -> Result<Option<RemotePrep>, RpcError> {
    let Some(raw) = p.get("remote_sources").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if !cfg.remote_sources {
        return Err(ae(
            Category::UnsupportedCapability,
            "remote_sources_off: set [assistant] remote_sources = true to accept source data from other machines",
        ));
    }
    if !matches!(
        op,
        Operation::Briefing | Operation::BackgroundSummary | Operation::Navigate
    ) {
        return Err(invalid(format!(
            "{} does not take remote_sources (briefing, background_summary and navigate do)",
            op.as_str()
        )));
    }
    let sources = remote::parse(raw).map_err(invalid)?;
    let mut cursors = data::cursors(server);
    let stale_after = cfg.remote_stale_seconds as i64 * 1000;
    let t = now();
    let mut inputs = vec![];
    let mut identities = vec![];
    let mut assessed = vec![];
    for s in sources {
        let fresh = remote::freshness(&s, t, stale_after);
        let gap = cursors.gap(&s);
        if fresh != Freshness::Unavailable {
            let id = s.workspace_identity();
            vk_assist::consent::check(grants, &id, resolved, op.as_str(), classes).map_err(|e| {
                rpc(AssistError::new(
                    e.category,
                    format!(
                        "{} (remote workspace {id}; grant it with `assistant.consent {{remote_workspace}}`)",
                        e.message
                    ),
                ))
            })?;
            identities.push(id);
            for it in &s.items {
                let mut object = it.object.clone();
                if let Some(o) = object.as_object_mut() {
                    o.insert("machine".into(), json!(s.machine));
                    o.insert("session".into(), json!(s.session));
                    if let Some(c) = &s.cursor {
                        o.insert("cursor".into(), json!(c));
                    }
                } else {
                    object =
                        json!({"machine": s.machine, "session": s.session, "cursor": s.cursor});
                }
                inputs.push(src(
                    &it.kind,
                    object,
                    format!(
                        "{} [{}/{}{}]",
                        it.label,
                        s.machine,
                        s.session,
                        if fresh == Freshness::Stale {
                            ", stale"
                        } else {
                            ""
                        }
                    ),
                    it.text.clone(),
                    s.observed_at_ms,
                ));
            }
            cursors.advance(&s);
        }
        assessed.push((s, fresh, gap));
    }
    let notes = remote::coverage_notes(&assessed, &server.opts.machine);
    Ok(Some(RemotePrep {
        inputs,
        notes,
        identities,
        cursors,
    }))
}
