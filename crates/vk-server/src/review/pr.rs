//! Pull-request evidence for review packages (15 §6.4, lane 3F; pure rules in
//! `vk_review::pr_evidence`).
//!
//! - `task.pr.observe {task, pr?, criteria?}`: an explicit, user-scope lookup through the
//!   authenticated `gh` CLI (never run automatically, never from a pane token). It records the
//!   provider/repository/PR identity, target branch, head revision, draft state, observation time
//!   and authorization scope as an immutable `pr_observation` row. Failed or offline lookups are
//!   recorded as such and stay unknown.
//! - `task.pr.claim {task, url}`: a pasted URL or an agent statement. Recorded and shown, never
//!   confirmation.
//! - `task.pr.list {task, subject?}`: the rows (and, for a subject, what each means for it).
//! - Review packages (`task.review.get`) carry a `pr` section and an `external_observation` item
//!   of evidence per observation for the selected subject: bound only to the committed subject
//!   whose head is exactly the observed PR head (a changed PR head invalidates the observation
//!   for the old subject), unknown when stale (`[review.pr] max_age_secs`, default 15 minutes).
//!   Merge and deployment observation are extension points only: a merged PR is shown but never
//!   a pass, and nothing here merges, deploys or comments.
//!
//! Config: `[review.pr] provider = "gh" | "off"`, `max_age_secs`. Tests point the `gh` binary at
//! a fake (`VIBEKE_GH_BIN` with `VIBEKE_TEST_HOOKS=1`).

use super::*;
use vk_review::Actor;
use vk_review::pr_evidence::{
    self as pe, ClaimSource, Lookup, PrAssessment, PrBinding, PrClaim, PrObservation,
};
use vk_review::readiness::EvidenceOutcome;

pub const METHODS: &[(&str, bool)] = &[
    ("task.pr.observe", true),
    ("task.pr.claim", true),
    ("task.pr.list", false),
];

/// An observation makes the lookup run under the user's credentials: not for pane tokens.
pub const PANE_FORBIDDEN: &[&str] = &["task.pr.observe"];

pub const K_OBS: &str = "pr_observation";
pub const K_CLAIM: &str = "pr_claim";

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct PrConfig {
    /// `gh` (the authenticated GitHub CLI) or `off`.
    pub provider: String,
    /// Observations older than this are unknown (0 = never stale).
    pub max_age_secs: u64,
}

impl Default for PrConfig {
    fn default() -> Self {
        PrConfig {
            provider: "gh".into(),
            max_age_secs: 900,
        }
    }
}

impl PrConfig {
    pub fn load() -> Self {
        let cfg = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c)
            .unwrap_or_default();
        Self::from_config(&cfg)
    }

    pub fn from_config(cfg: &vk_config::Config) -> Self {
        cfg.extra
            .get("review")
            .and_then(|t| t.get("pr"))
            .and_then(|t| serde_json::to_value(t).ok())
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    fn max_age_ms(&self) -> i64 {
        (self.max_age_secs as i64).saturating_mul(1000)
    }
}

// ---- storage --------------------------------------------------------------------------------

pub fn observations_of(c: &Core, task: &str) -> Vec<PrObservation> {
    let mut v: Vec<PrObservation> = c.store.load_by_task(K_OBS, task).unwrap_or_default();
    v.sort_by(|a, b| {
        a.observed_at_ms
            .cmp(&b.observed_at_ms)
            .then(a.id.cmp(&b.id))
    });
    v
}

pub fn claims_of(c: &Core, task: &str) -> Vec<PrClaim> {
    let mut v: Vec<PrClaim> = c.store.load_by_task(K_CLAIM, task).unwrap_or_default();
    v.sort_by(|a, b| a.claimed_at_ms.cmp(&b.claimed_at_ms).then(a.id.cmp(&b.id)));
    v
}

/// Part of the acceptance state token: which observations and claims exist.
pub fn token_part(c: &Core, task: &str) -> String {
    let mut ids: Vec<String> = observations_of(c, task).into_iter().map(|o| o.id).collect();
    ids.extend(claims_of(c, task).into_iter().map(|o| o.id));
    ids.sort();
    format!("pr {ids:?}\n")
}

// ---- lookup ---------------------------------------------------------------------------------

/// Fake backend for in-process tests: the answer for a checkout (no `gh` is run).
#[cfg(test)]
pub(crate) fn test_lookups() -> &'static Mutex<HashMap<PathBuf, Lookup>> {
    static M: OnceLock<Mutex<HashMap<PathBuf, Lookup>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// Run the lookup (blocking: spawns `gh`).
pub fn lookup_blocking(checkout: &Path, pr: Option<&str>) -> Lookup {
    #[cfg(test)]
    if let Some(l) = test_lookups()
        .lock()
        .unwrap()
        .get(&std::fs::canonicalize(checkout).unwrap_or_else(|_| checkout.to_path_buf()))
    {
        return l.clone();
    }
    match vk_tasks::fetch_pr_evidence_json(checkout, pr) {
        vk_tasks::PrJson::Found(json) => match pe::parse_gh_pr_view(&json) {
            Ok(facts) => Lookup::Observed { facts },
            Err(reason) => Lookup::Failed { reason },
        },
        vk_tasks::PrJson::NoPr => Lookup::NoPr,
        vk_tasks::PrJson::Unavailable(reason) => Lookup::Failed { reason },
    }
}

// ---- review package integration -------------------------------------------------------------

fn lookup_json(l: &Lookup) -> Value {
    match l {
        Lookup::Observed { facts } => json!({
            "kind": "observed",
            "pr": facts.identity.key(),
            "url": facts.identity.url,
            "provider": facts.identity.provider,
            "repository": facts.identity.repository,
            "number": facts.identity.number,
            "target_branch": facts.target_branch,
            "head_branch": facts.head_branch,
            "head_sha": facts.head_sha,
            "state": facts.state,
            "draft": facts.draft,
            "checks": facts.checks,
            "review_decision": facts.review_decision,
        }),
        Lookup::NoPr => json!({"kind": "no_pr"}),
        Lookup::Failed { reason } => json!({"kind": "failed", "reason": reason}),
    }
}

fn assessment_json(a: &PrAssessment) -> Value {
    json!({
        "binding": match a.binding {
            PrBinding::Bound => "bound",
            PrBinding::Unbound { .. } => "unbound",
        },
        "unbound_reason": match a.binding {
            PrBinding::Unbound { reason } => json!({"reason": reason, "text": reason.text()}),
            PrBinding::Bound => Value::Null,
        },
        "outcome": match a.outcome {
            EvidenceOutcome::Passed => "passed",
            EvidenceOutcome::Failed => "failed",
            EvidenceOutcome::Unknown => "unknown",
        },
        "summary": a.summary,
        "stale": a.stale,
    })
}

fn observation_row(o: &PrObservation, current: bool, a: Option<&PrAssessment>) -> Value {
    let mut v = serde_json::to_value(o).unwrap_or(Value::Null);
    v["lookup"] = lookup_json(&o.lookup);
    v["current"] = json!(current);
    if let Some(a) = a {
        v["assessment"] = assessment_json(a);
    }
    v
}

/// Evidence and the package's `pr` section for the selected subject.
pub fn review_evidence(
    server: &Server,
    task: &str,
    subject: Option<&ChangeSubject>,
    intent: Option<&TaskIntent>,
) -> (Vec<Evidence>, Value) {
    let (obs, claims) = server.with_core(|c| (observations_of(c, task), claims_of(c, task)));
    let cfg = PrConfig::load();
    let at = now();
    let current = pe::current_ids(&obs);
    let external = pe::external_criteria(intent);
    let mut evidence = Vec::new();
    let mut rows = Vec::new();
    for o in &obs {
        let a = pe::assess(o, subject, at, cfg.max_age_ms());
        evidence.push(pe::to_evidence(o, &a, subject));
        rows.push(observation_row(o, current.contains(&o.id), Some(&a)));
    }
    let mut claim_rows = Vec::new();
    for cl in &claims {
        if !external.is_empty() {
            evidence.push(cl.to_evidence(&external));
        }
        let mut v = serde_json::to_value(cl).unwrap_or(Value::Null);
        v["label"] = json!(cl.label());
        v["confirmed"] = json!(false);
        claim_rows.push(v);
    }
    let section = json!({
        "provider": cfg.provider,
        "max_age_secs": cfg.max_age_secs,
        "observations": rows,
        "claims": claim_rows,
        "observe": {"method": "task.pr.observe", "available": cfg.provider != "off"},
        "note": "A pull request supports an external criterion only through an observation whose head is this exact revision. Pasted URLs and agent statements are claims, not confirmation. Merge and deployment are not observed.",
    });
    (evidence, section)
}

// ---- API ------------------------------------------------------------------------------------

pub(super) async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "task.pr.observe" => observe_api(server, ctx, p).await,
        "task.pr.claim" => claim_api(server, ctx, p),
        "task.pr.list" => list_api(server, ctx, p),
        _ => return None,
    })
}

async fn observe_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.pr.observe";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let task = tracking::find_task(server, task_id)?;
    let cfg = PrConfig::load();
    if cfg.provider == "off" {
        return Err(err(
            ErrorKind::Unsupported,
            "pull request lookup is disabled ([review.pr] provider = \"off\")",
        ));
    }
    let pr_ref = s(p, "pr").map(str::to_string);
    if let Some(r) = &pr_ref {
        if !vk_tasks::valid_pr_ref(r) {
            return Err(invalid("`pr` must be a pull request number, URL or branch"));
        }
        if r.contains("://") && pe::parse_pr_url(r).is_none() {
            return Err(invalid("`pr` is not a pull request URL"));
        }
    }
    let intent = task
        .intent_revision
        .and_then(|r| tracking::intent_at(server, &task.id, r));
    let external = pe::external_criteria(intent.as_ref());
    let criteria: Vec<String> = match p.get("criteria") {
        None | Some(Value::Null) => external,
        Some(Value::Array(a)) => {
            let mut v = Vec::new();
            for x in a {
                let id = x
                    .as_str()
                    .ok_or_else(|| invalid("`criteria` must be a list of criterion ids"))?;
                if !external.iter().any(|e| e == id) {
                    return Err(invalid(format!(
                        "criterion {id} is not an external criterion of this task's intent"
                    )));
                }
                v.push(id.to_string());
            }
            v
        }
        Some(_) => return Err(invalid("`criteria` must be a list of criterion ids")),
    };
    let path = checkout_of(&task).ok_or_else(|| {
        conflict(
            "checkout_unavailable",
            "the task's checkout is unavailable; nothing to look up",
        )
    })?;
    let (path2, ref2) = (path.clone(), pr_ref.clone());
    let lookup = blocking(move || lookup_blocking(&path2, ref2.as_deref())).await?;

    let obs = PrObservation {
        id: crate::core::ulid(),
        task: task.id.clone(),
        requested: pr_ref,
        lookup,
        observed_at_ms: now(),
        authorization_scope: "gh_cli".into(),
        provider: "gh".into(),
        observed_by: tracking::user(ctx),
        criterion_ids: criteria,
    };
    let label = match &obs.lookup {
        Lookup::Observed { facts } => format!(
            "{} · head {} · {}",
            facts.identity.key(),
            &facts.head_sha[..facts.head_sha.len().min(10)],
            serde_json::to_value(facts.state)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default()
        ),
        Lookup::NoPr => "No pull request found for this branch".to_string(),
        Lookup::Failed { reason } => format!("Lookup failed ({reason}); outcome unknown"),
    };
    let result = json!({
        "observation": observation_row(&obs, true, None),
        "label": label,
        "note": "Observed through your gh login; read-only. It supports an external criterion only for the revision whose head is the pull request head.",
    });
    {
        let mut c = server.core.lock().unwrap();
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            return r;
        }
        let prev = observations_of(&c, &task.id)
            .into_iter()
            .rev()
            .find(|o| o.key().is_some() && o.key() == obs.key());
        let mut tx = Tx::new();
        tx.m.close(K_OBS, &obs.id, None, &obs);
        let (head, state, draft, checks, pr_key, reason) = match &obs.lookup {
            Lookup::Observed { facts } => (
                Some(facts.head_sha.clone()),
                serde_json::to_value(facts.state).ok(),
                Some(facts.draft),
                serde_json::to_value(facts.checks).ok(),
                Some(facts.identity.key()),
                None,
            ),
            Lookup::NoPr => (None, None, None, None, None, None),
            Lookup::Failed { reason } => (None, None, None, None, None, Some(reason.clone())),
        };
        tx.event_by(
            "review.pr_observed",
            json!({"task": task.id}),
            json!({"kind": "user", "id": obs.observed_by.id}),
            json!({
                "observation": obs.id,
                "lookup": match &obs.lookup {
                    Lookup::Observed { .. } => "observed",
                    Lookup::NoPr => "no_pr",
                    Lookup::Failed { .. } => "failed",
                },
                "pr": pr_key,
                "head": head,
                "state": state,
                "draft": draft,
                "checks": checks,
                "reason": reason,
            }),
        );
        if let Some(prev) = &prev
            && pe::head_changed(prev, &obs)
        {
            tx.event(
                "review.pr_head_changed",
                json!({"task": task.id}),
                json!({
                    "pr": obs.key(),
                    "from": prev.facts().map(|f| f.head_sha.clone()),
                    "to": obs.facts().map(|f| f.head_sha.clone()),
                    "observation": obs.id,
                }),
            );
        }
        receipts::record(&mut tx, ctx, M, p, &result);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    spawn_refresh(server, &task.id);
    Ok(result)
}

fn claim_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    const M: &str = "task.pr.claim";
    if let Some(r) = receipts::replay(server, ctx, M, p) {
        return r;
    }
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let task = tracking::find_task(server, task_id)?;
    let url = req(p, "url")?.trim();
    if url.is_empty() || url.len() > 2048 || url.chars().any(char::is_control) {
        return Err(invalid(
            "`url` must be a short, single-line pull request URL",
        ));
    }
    let text = s(p, "text").map(|t| t.chars().take(500).collect::<String>());
    let (source, by) = match &ctx.pane_scope {
        Some(pane) => (ClaimSource::AgentStatement, Actor::agent(pane.clone())),
        None => (ClaimSource::PastedUrl, tracking::user(ctx)),
    };
    let claim = PrClaim::new(crate::core::ulid(), task.id.clone(), url, source, by, text);
    let result = json!({
        "claim": claim,
        "label": claim.label(),
        "note": "A claim is shown with the review but is not confirmation; use task.pr.observe to look the pull request up.",
    });
    {
        let mut c = server.core.lock().unwrap();
        if let Some(r) = receipts::replay_in(&c, ctx, M, p) {
            return r;
        }
        let mut tx = Tx::new();
        tx.m.close(K_CLAIM, &claim.id, None, &claim);
        tx.event(
            "review.pr_claimed",
            json!({"task": task.id}),
            json!({
                "claim": claim.id,
                "url": claim.url,
                "pr": claim.identity.as_ref().map(|i| i.key()),
                "source": claim.source,
            }),
        );
        receipts::record(&mut tx, ctx, M, p, &result);
        server.commit(&mut c, tx).map_err(internal)?;
    }
    spawn_refresh(server, &task.id);
    Ok(result)
}

fn list_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let task_id = req(p, "task")?;
    authorize_task(server, ctx, task_id)?;
    let task = tracking::find_task(server, task_id)?;
    let (obs, claims) =
        server.with_core(|c| (observations_of(c, &task.id), claims_of(c, &task.id)));
    let subject: Option<ChangeSubject> = match s(p, "subject") {
        None => None,
        Some(id) => Some(
            server
                .with_core(|c| c.store.get::<ChangeSubject>(K_SUBJECT, id).ok().flatten())
                .ok_or_else(|| not_found("subject", id))?,
        ),
    };
    let cfg = PrConfig::load();
    let at = now();
    let current = pe::current_ids(&obs);
    let rows: Vec<Value> = obs
        .iter()
        .map(|o| {
            let a = subject
                .as_ref()
                .map(|s| pe::assess(o, Some(s), at, cfg.max_age_ms()));
            observation_row(o, current.contains(&o.id), a.as_ref())
        })
        .collect();
    let claim_rows: Vec<Value> = claims
        .iter()
        .map(|cl| {
            let mut v = serde_json::to_value(cl).unwrap_or(Value::Null);
            v["label"] = json!(cl.label());
            v["confirmed"] = json!(false);
            v
        })
        .collect();
    Ok(json!({
        "task": task.id,
        "observations": rows,
        "claims": claim_rows,
        "provider": cfg.provider,
        "max_age_secs": cfg.max_age_secs,
    }))
}
