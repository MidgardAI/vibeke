//! Lane 2C, spec 15 §11 / 09: `forget` purges spec-15 **derived objects** too.
//!
//! "`forget` removes scoped drafts, messages, derived packages and cached excerpts as well as
//! underlying sources; retained references become unavailable rather than resurrecting purged
//! content. Required evidence purged from a package changes readiness to unknown."
//!
//! `task.review.forget {task | pane | workspace | before | all, dry_run?}` (full scope only)
//! and every `scrollback.forget` (`vibeke forget`, same scope) purge, for the tasks and runs in
//! scope:
//!
//! - task messages (clarifications): the text; the delivery record stays;
//! - intent revisions: the bounded source excerpt (the confirmed intent itself is the user's
//!   record and stays);
//! - turn records: the bounded prompt copy and last agent message;
//! - observed tool items: the command and claim text (exit code and times stay);
//! - reviewer requests: the prompt (its digest stays); review notes and human-review notes: the
//!   text;
//! - check runs: the log file (outcome metadata stays);
//! - cached review projections: deleted (rebuilt only from what is still authorized).
//!
//! Every purged object gets a tombstone (`review_purged`), so a later package shows the item as
//! **content purged** instead of rebuilding it from a cache, and evidence whose content was
//! purged (observed commands of purged runs, check runs whose logs were purged) counts as
//! `unknown`: a required criterion it supported is no longer supported. Events carry scope and
//! counts only (`review.purged`), never text.

use super::*;

pub(super) const K_PURGED: &str = "review_purged";

/// What was purged (tombstone, keyed `kind:id`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tombstone {
    pub key: String,
    /// `message | intent_excerpt | turn | tool_item | reviewer_prompt | note | human_note |
    /// check_log | projection`.
    pub kind: String,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    pub at_ms: i64,
}

/// The resolved scope of a purge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    Task(String),
    /// Pane ids (a pane, or every pane of a workspace) plus the workspace, if one was named.
    Panes {
        panes: Vec<String>,
        workspace: Option<String>,
    },
    /// Objects created before this time (epoch ms).
    Before(i64),
}

impl Scope {
    fn json(&self) -> Value {
        match self {
            Scope::All => json!({"all": true}),
            Scope::Task(t) => json!({"task": t}),
            Scope::Panes { panes, workspace } => json!({"panes": panes, "workspace": workspace}),
            Scope::Before(t) => json!({"before": t}),
        }
    }
    fn cutoff(&self) -> Option<i64> {
        match self {
            Scope::Before(t) => Some(*t),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Report {
    pub tasks: usize,
    pub runs: usize,
    pub messages: usize,
    pub intent_excerpts: usize,
    pub turns: usize,
    pub tool_items: usize,
    pub reviewer_prompts: usize,
    pub notes: usize,
    pub human_notes: usize,
    pub check_logs: usize,
    pub projections: usize,
    /// Tasks whose package must be rebuilt.
    #[serde(skip)]
    pub touched: Vec<String>,
}

impl Report {
    fn total(&self) -> usize {
        self.messages
            + self.intent_excerpts
            + self.turns
            + self.tool_items
            + self.reviewer_prompts
            + self.notes
            + self.human_notes
            + self.check_logs
            + self.projections
    }
}

const PURGED_TEXT: &str = "";

/// Runs and tasks the scope covers (runs: live and ended).
fn resolve(c: &Core, scope: &Scope) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut runs: Vec<AgentRun> = c.model.runs.clone();
    for r in c
        .store
        .load_closed::<AgentRun>("run", 10_000)
        .unwrap_or_default()
    {
        if !runs.iter().any(|x| x.id == r.id) {
            runs.push(r);
        }
    }
    let mut tasks: Vec<Task> = c.model.tasks.clone();
    for t in c
        .store
        .load_closed::<Task>("task", 10_000)
        .unwrap_or_default()
    {
        if !tasks.iter().any(|x| x.id == t.id) {
            tasks.push(t);
        }
    }
    let bindings: Vec<TaskRunBinding> = c
        .store
        .load_closed::<TaskRunBinding>(tracking::K_BINDING, 100_000)
        .unwrap_or_default()
        .into_iter()
        .chain(tracking::bindings(c))
        .collect();
    match scope {
        Scope::All | Scope::Before(_) => (
            runs.iter().map(|r| r.id.clone()).collect(),
            tasks.iter().map(|t| t.id.clone()).collect(),
        ),
        Scope::Task(t) => {
            let rs = bindings
                .iter()
                .filter(|b| &b.task_id == t)
                .map(|b| b.run_id.clone())
                .collect();
            (rs, [t.clone()].into_iter().collect())
        }
        Scope::Panes { panes, workspace } => {
            let rs: BTreeSet<String> = runs
                .iter()
                .filter(|r| panes.contains(&r.pane))
                .map(|r| r.id.clone())
                .collect();
            let mut ts: BTreeSet<String> = bindings
                .iter()
                .filter(|b| rs.contains(&b.run_id))
                .map(|b| b.task_id.clone())
                .collect();
            if let Some(w) = workspace {
                ts.extend(
                    tasks
                        .iter()
                        .filter(|t| t.workspace.as_deref() == Some(w.as_str()))
                        .map(|t| t.id.clone()),
                );
            }
            (rs, ts)
        }
    }
}

/// Purge (or, with `dry_run`, count) the derived objects in scope. Blocking (file removal).
pub fn run(server: &Server, scope: &Scope, dry_run: bool) -> Report {
    let cutoff = scope.cutoff();
    let old = |t: i64| cutoff.is_none_or(|c| t < c);
    let now_ms = now();
    let mut rep = Report::default();
    let mut logs: Vec<PathBuf> = vec![];
    let mut c = server.core.lock().unwrap();
    let (runs, tasks) = resolve(&c, scope);
    rep.runs = runs.len();
    rep.tasks = tasks.len();
    let mut tx = Tx::new();
    let tomb = |tx: &mut Tx, kind: &str, id: &str, task: Option<&str>, run: Option<&str>| {
        let key = format!("{kind}:{id}");
        tx.m.put(
            K_PURGED,
            &key,
            None,
            &Tombstone {
                key: key.clone(),
                kind: kind.into(),
                id: id.into(),
                task: task.map(str::to_string),
                run: run.map(str::to_string),
                at_ms: now_ms,
            },
        );
    };
    // Task messages.
    for t in &tasks {
        for mut m in by_task::<tracking::TaskMessage>(&c, tracking::K_MESSAGE, t) {
            if m.text.is_empty() || !old(m.created_at_ms) {
                continue;
            }
            rep.messages += 1;
            if !dry_run {
                m.text = PURGED_TEXT.into();
                m.detail = Some("content purged by forget".into());
                tx.m.put(tracking::K_MESSAGE, &m.id, None, &m);
                tomb(&mut tx, "message", &m.id, Some(t), Some(&m.run));
            }
        }
        // Intent source excerpts (closed history records; keyed by `task_id`, not `task`).
        for mut i in c
            .store
            .load_by_field::<vk_review::intent::TaskIntent>(tracking::K_INTENT, "$.task_id", t)
            .unwrap_or_default()
        {
            if i.source_excerpt.is_none() || !old(i.confirmed_at_ms) {
                continue;
            }
            rep.intent_excerpts += 1;
            if !dry_run {
                i.source_excerpt = None;
                let id = format!("{t}:{}", i.revision);
                tx.m.close(tracking::K_INTENT, &id, None, &i);
                tomb(&mut tx, "intent_excerpt", &id, Some(t), None);
            }
        }
        // Reviewer prompts.
        for mut r in by_task::<t4::ReviewerRequest>(&c, t4::K_REVREQ, t) {
            if r.prompt.is_empty() || !old(r.created_at_ms) {
                continue;
            }
            rep.reviewer_prompts += 1;
            if !dry_run {
                r.prompt = PURGED_TEXT.into();
                tx.m.put(t4::K_REVREQ, &r.id, None, &r);
                tomb(&mut tx, "reviewer_prompt", &r.id, Some(t), r.run.as_deref());
            }
        }
        // Review notes.
        for mut n in by_task::<vk_review::reviewer::ReviewNote>(&c, t4::K_NOTE, t) {
            if n.text.is_empty() || !old(n.created_at_ms) {
                continue;
            }
            rep.notes += 1;
            if !dry_run {
                n.text = PURGED_TEXT.into();
                tx.m.put(t4::K_NOTE, &n.id, None, &n);
                tomb(&mut tx, "note", &n.id, Some(t), n.run.as_deref());
            }
        }
        // Human-review notes.
        for mut h in human::reviews_of(&c, t) {
            if h.note.is_none() || !old(h.at_ms) {
                continue;
            }
            rep.human_notes += 1;
            if !dry_run {
                h.note = None;
                h.purged_at_ms = Some(now_ms);
                tx.m.close(human::K_HUMAN, &h.id, None, &h);
                tomb(&mut tx, "human_note", &h.id, Some(t), None);
            }
        }
        // Check run logs.
        for mut r in by_task::<CheckRunRec>(&c, K_CHECK, t) {
            if r.run.log_path.is_none() || !r.run.state.is_terminal() {
                continue;
            }
            if !old(r.run.ended_at_ms.unwrap_or(now_ms)) {
                continue;
            }
            rep.check_logs += 1;
            if !dry_run {
                if let Some(p) = r.run.log_path.take() {
                    logs.push(PathBuf::from(p));
                }
                r.run.log_bytes = 0;
                put_check(&mut tx, &r);
                tomb(&mut tx, "check_log", &r.run.id, Some(t), None);
            }
        }
        // Cached projection (rebuilt from authorized inputs only).
        if cutoff.is_none() && c.store.get::<Value>(K_PROJ, t).ok().flatten().is_some() {
            rep.projections += 1;
            if !dry_run {
                tx.m.delete(K_PROJ, t);
            }
        }
    }
    // Turn prompt copies and tool items of the runs in scope.
    for r in &runs {
        for mut tr in c
            .store
            .load_by_field::<tracking::TurnRecord>(tracking::K_TURN, "$.run", r)
            .unwrap_or_default()
        {
            if (tr.prompt.is_empty() && tr.last_message.is_none()) || !old(tr.started_at_ms) {
                continue;
            }
            rep.turns += 1;
            if !dry_run {
                tr.prompt = PURGED_TEXT.into();
                tr.last_message = None;
                if tr.ended_at_ms.is_some() {
                    tx.m.close(tracking::K_TURN, &tr.id, None, &tr);
                } else {
                    tx.m.put(tracking::K_TURN, &tr.id, None, &tr);
                }
                tomb(&mut tx, "turn", &tr.id, None, Some(r));
            }
        }
        for mut it in c
            .store
            .load_by_field::<ToolRecord>(tracking::K_ITEM, "$.run_id", r)
            .unwrap_or_default()
        {
            if (it.command.is_none() && it.text.is_none()) || !old(it.started_at_ms.unwrap_or(0)) {
                continue;
            }
            rep.tool_items += 1;
            if !dry_run {
                it.command = None;
                it.text = None;
                let id = it.item_id.clone();
                if it.ended_at_ms.is_some() {
                    tx.m.close(tracking::K_ITEM, &id, None, &it);
                } else {
                    tx.m.put(tracking::K_ITEM, &id, None, &it);
                }
                tomb(&mut tx, "tool_item", &id, None, Some(r));
            }
        }
    }
    if !dry_run && rep.total() > 0 {
        tx.event(
            "review.purged",
            json!({"scope": scope.json()}),
            serde_json::to_value(&rep).unwrap_or(Value::Null),
        );
        let _ = server.commit(&mut c, tx);
    }
    drop(c);
    for p in logs {
        let _ = std::fs::remove_file(p);
    }
    if !dry_run && rep.total() > 0 {
        rep.touched = tasks.into_iter().collect();
    }
    rep
}

fn tombstones(c: &Core) -> Vec<Tombstone> {
    c.store.load::<Tombstone>(K_PURGED).unwrap_or_default()
}

/// Evidence whose content `forget` purged becomes `unknown` (15 §11): check runs whose logs
/// were purged and observed commands/claims of runs whose tool items or turns were purged.
pub(super) fn degrade_purged(server: &Server, task: &str, evidence: &mut [Evidence]) {
    let ts = server.with_core(|c| tombstones(c));
    if ts.is_empty() {
        return;
    }
    let checks: HashSet<&str> = ts
        .iter()
        .filter(|t| t.kind == "check_log" && t.task.as_deref() == Some(task))
        .map(|t| t.id.as_str())
        .collect();
    let runs: HashSet<&str> = ts
        .iter()
        .filter(|t| matches!(t.kind.as_str(), "tool_item" | "turn"))
        .filter_map(|t| t.run.as_deref())
        .collect();
    for e in evidence.iter_mut() {
        let purged = match e.category {
            vk_review::readiness::EvidenceCategory::VibekeVerification => {
                checks.contains(e.id.as_str())
            }
            vk_review::readiness::EvidenceCategory::Observed
            | vk_review::readiness::EvidenceCategory::AgentClaim => e
                .actor
                .as_ref()
                .is_some_and(|a| runs.contains(a.id.as_str())),
            _ => false,
        };
        if purged {
            e.outcome = vk_review::readiness::EvidenceOutcome::Unknown;
            e.summary = Some("content purged by forget; outcome no longer inspectable".into());
        }
    }
}

/// The package's `purged` section: what is unavailable because it was forgotten.
pub(super) fn package_json(server: &Server, task: &str) -> Value {
    let ts: Vec<Tombstone> = server.with_core(|c| {
        tombstones(c)
            .into_iter()
            .filter(|t| t.task.as_deref() == Some(task))
            .collect()
    });
    if ts.is_empty() {
        return Value::Null;
    }
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for t in &ts {
        *counts.entry(t.kind.clone()).or_default() += 1;
    }
    json!({
        "counts": counts,
        "note": "Some content of this task was purged by forget; it is shown as unavailable and evidence that depended on it counts as unknown.",
    })
}

fn parse_scope(server: &Server, ctx: &Ctx, p: &Value) -> Result<Scope, RpcError> {
    let all = p.get("all").and_then(Value::as_bool).unwrap_or(false);
    let given = [
        all,
        p.get("task").is_some(),
        p.get("pane").is_some(),
        p.get("workspace").is_some(),
        p.get("before").is_some(),
    ]
    .iter()
    .filter(|x| **x)
    .count();
    if given != 1 {
        return Err(invalid(
            "task.review.forget needs exactly one of task, pane, workspace, before or all=true",
        ));
    }
    if all {
        return Ok(Scope::All);
    }
    if let Some(t) = s(p, "task") {
        return Ok(Scope::Task(tracking::find_task(server, t)?.id));
    }
    if let Some(pn) = s(p, "pane") {
        let id = crate::api::resolve_pane(server, ctx, Some(pn))
            .map(|x| x.id)
            .unwrap_or_else(|_| pn.to_string());
        return Ok(Scope::Panes {
            panes: vec![id],
            workspace: None,
        });
    }
    if let Some(w) = s(p, "workspace") {
        let panes = server.with_core(|c| {
            c.model
                .panes
                .iter()
                .filter(|x| x.workspace == w)
                .map(|x| x.id.clone())
                .collect()
        });
        return Ok(Scope::Panes {
            panes,
            workspace: Some(w.to_string()),
        });
    }
    let before = crate::desk::time_param(p, "before")?.unwrap_or_default();
    Ok(Scope::Before(before))
}

/// `task.review.forget {task | pane | workspace | before | all, dry_run?}` (full scope only).
pub(super) async fn forget_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            "forget is a user action (pane tokens can't purge)",
        ));
    }
    let scope = parse_scope(server, ctx, p)?;
    let dry_run = p.get("dry_run").and_then(Value::as_bool).unwrap_or(false);
    let srv = server.clone();
    let sc = scope.clone();
    let rep = blocking(move || run(&srv, &sc, dry_run)).await?;
    for t in &rep.touched {
        spawn_refresh(server, t);
    }
    Ok(json!({"scope": scope.json(), "dry_run": dry_run, "purged": rep}))
}

/// Hook from `scrollback.forget` (`vibeke forget`): the same scope purges spec-15 derived
/// objects. `scope` is that method's resolved scope JSON (`{pane} | {workspace} | {before} |
/// {all}`) and `panes` its resolved pane ids.
pub fn on_scrollback_forget(
    server: &Server,
    scope: &Value,
    panes: Option<&[String]>,
    dry_run: bool,
) -> Value {
    let sc = if scope.get("all").and_then(Value::as_bool) == Some(true) {
        Scope::All
    } else if let Some(t) = scope.get("before").and_then(Value::as_i64) {
        Scope::Before(t)
    } else {
        Scope::Panes {
            panes: panes.map(<[String]>::to_vec).unwrap_or_default(),
            workspace: scope
                .get("workspace")
                .and_then(Value::as_str)
                .map(str::to_string),
        }
    };
    serde_json::to_value(run(server, &sc, dry_run)).unwrap_or(Value::Null)
}
