//! Attention extras (15 §8.1, §8.3), lane 2C: native deadlines, server-side batching, the
//! **Also working** footer and inbox notifications that respect presence and quiet hours.
//!
//! - **Native deadlines.** An interaction can carry a deadline: one the harness reports
//!   (`deadline_ms` / `expires_at_ms` absolute, or `timeout_ms` / `timeout` relative in an
//!   adapter payload, or [`record_native_deadline`] from an adapter), source `native`; or the
//!   end of Vibeke's own hook gate (the hook's 30-minute timeout, after which the harness shows
//!   its own dialog), source `gate`, which only counts while the gate is held. Deadlines live
//!   in a side table (`interaction_deadline`), so the `Interaction` model is unchanged. An
//!   interaction is class 2 when at most `ui.interactions.deadline_window` (default `60s`)
//!   remains. A passed **native** deadline is reconciled at the source: the interaction is
//!   closed as `expired` by the attention watcher, so a cached card can't answer it.
//! - **Batching.** `attention.list` marks groups of equivalent native approvals
//!   (`vk_review::attention::batchable`, computed from server facts: harness, tool, the raw
//!   command byte for byte, resource paths, the pane's isolation level and network profile,
//!   and the workspace root as policy scope) and `attention.batch {interaction}` returns the
//!   group with each member's decision revision. Members are still answered one by one with
//!   `interaction.answer` (`expected_decision_rev`), so each has its own delivery outcome;
//!   there is no second answer API (15 §10.2).
//! - **Also working.** Busy runs without an open interaction, for the inbox footer (they stay
//!   out of the ranked list).
//! - **Notifications.** A watcher (event-driven on model changes, plus a timer only while
//!   deadlines or snoozes are pending) turns newly urgent items (delivery problems, deadlines
//!   approaching), failed checks and tasks that became **Ready for your review** into
//!   notifications through the ordinary pipeline (`Server::notify`): `notifications.on`
//!   (`deadline`, `review`), presence (the item's pane focused in an attached client) and
//!   quiet hours apply there. Snoozed items stay quiet unless a material change woke them.
//!   Open interactions are not re-notified (they already were when they opened).

use super::*;
use vk_proto::model::InteractionKind as MK;

pub(super) const K_DEADLINE: &str = "interaction_deadline";

/// A deadline of one interaction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeadlineRec {
    pub interaction: String,
    pub deadline_ms: i64,
    /// `native` (the harness's own request timeout) or `gate` (Vibeke's hook gate ends; the
    /// native dialog shows afterwards).
    pub source: String,
    pub recorded_at_ms: i64,
}

// ---- configuration ------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttnCfg {
    pub deadline_window_ms: i64,
    pub also_working: bool,
}

impl Default for AttnCfg {
    fn default() -> Self {
        AttnCfg {
            deadline_window_ms: 60_000,
            also_working: true,
        }
    }
}

fn cfg_override() -> &'static Mutex<Option<AttnCfg>> {
    static O: OnceLock<Mutex<Option<AttnCfg>>> = OnceLock::new();
    O.get_or_init(Default::default)
}

/// Tests set the configuration directly instead of through config.toml.
#[cfg(test)]
pub(crate) fn set_test_cfg(c: Option<AttnCfg>) {
    *cfg_override().lock().unwrap() = c;
}

/// `ui.interactions.deadline_window` and `ui.inbox.also_working`, re-read at most every 2 s.
pub fn cfg() -> AttnCfg {
    if let Some(c) = *cfg_override().lock().unwrap() {
        return c;
    }
    static CACHE: OnceLock<Mutex<Option<(Instant, AttnCfg)>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some((at, c)) = *cache.lock().unwrap()
        && at.elapsed() < Duration::from_secs(2)
    {
        return c;
    }
    let c = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| AttnCfg {
            deadline_window_ms: c.ui.interactions.deadline_window.0.as_millis() as i64,
            also_working: c.ui.inbox.also_working,
        })
        .unwrap_or_default();
    *cache.lock().unwrap() = Some((Instant::now(), c));
    c
}

/// Ranking preferences from the configuration.
pub fn prefs() -> att::AttentionPrefs {
    att::AttentionPrefs {
        deadline_window_ms: cfg().deadline_window_ms.max(0),
    }
}

// ---- deadlines ----------------------------------------------------------------------------------

/// A native deadline in an adapter payload: absolute `deadline_ms` / `expires_at_ms` (epoch
/// ms), or relative `timeout_ms` / `timeout` (seconds) from `now_ms`.
pub fn deadline_from_payload(p: &Value, now_ms: i64) -> Option<i64> {
    let abs = ["deadline_ms", "expires_at_ms"]
        .iter()
        .find_map(|k| p.get(*k).and_then(Value::as_i64))
        .filter(|d| *d > 0);
    if abs.is_some() {
        return abs;
    }
    if let Some(ms) = p
        .get("timeout_ms")
        .and_then(Value::as_i64)
        .filter(|v| *v > 0)
    {
        return Some(now_ms + ms);
    }
    p.get("timeout")
        .and_then(Value::as_f64)
        .filter(|v| *v > 0.0)
        .map(|s| now_ms + (s * 1000.0) as i64)
}

fn put_deadline(server: &Server, rec: DeadlineRec) {
    let mut c = server.core.lock().unwrap();
    let mut tx = Tx::new();
    tx.m.put(K_DEADLINE, &rec.interaction, None, &rec);
    let _ = server.commit(&mut c, tx);
}

/// Record the harness's own deadline for an interaction (adapters that know it).
pub fn record_native_deadline(server: &Server, interaction: &str, deadline_ms: i64) {
    put_deadline(
        server,
        DeadlineRec {
            interaction: interaction.to_string(),
            deadline_ms,
            source: "native".into(),
            recorded_at_ms: now(),
        },
    );
}

/// Hook from `agents::gate` once a hook interaction is open and not answered by policy: a
/// native deadline from the payload, else (gate mode) the end of the hook gate.
pub fn on_gate_opened(server: &Server, interaction: &str, p: &Value, gate: Option<Duration>) {
    let now_ms = now();
    let rec = match deadline_from_payload(p, now_ms) {
        Some(d) => DeadlineRec {
            interaction: interaction.to_string(),
            deadline_ms: d,
            source: "native".into(),
            recorded_at_ms: now_ms,
        },
        None => match gate {
            Some(g) => DeadlineRec {
                interaction: interaction.to_string(),
                deadline_ms: now_ms + g.as_millis() as i64,
                source: "gate".into(),
                recorded_at_ms: now_ms,
            },
            None => return,
        },
    };
    put_deadline(server, rec);
}

/// The deadline that applies to an open interaction now: a native one, or the gate's while
/// the gate is still held.
pub(super) fn effective_deadline<'a>(
    recs: &'a HashMap<String, DeadlineRec>,
    i: &Interaction,
) -> Option<&'a DeadlineRec> {
    recs.get(&i.id)
        .filter(|d| d.source == "native" || (d.source == "gate" && i.gate))
}

pub(super) fn deadlines(c: &Core) -> HashMap<String, DeadlineRec> {
    c.store
        .load::<DeadlineRec>(K_DEADLINE)
        .unwrap_or_default()
        .into_iter()
        .map(|d| (d.interaction.clone(), d))
        .collect()
}

/// Close open interactions whose **native** deadline passed (`expired`), and drop deadline
/// records of interactions that are no longer open. Returns the expired interaction ids.
pub fn reconcile_deadlines(server: &Server, now_ms: i64) -> Vec<String> {
    let (expired, gone): (Vec<String>, Vec<String>) = server.with_core(|c| {
        let recs = deadlines(c);
        let mut expired = vec![];
        let mut gone = vec![];
        for (id, d) in &recs {
            match c.interaction(id) {
                Some(i) if i.status == InteractionStatus::Open => {
                    if d.source == "native" && d.deadline_ms <= now_ms {
                        expired.push(id.clone());
                    }
                }
                _ => gone.push(id.clone()),
            }
        }
        (expired, gone)
    });
    for id in &expired {
        crate::agents::close_interaction(
            server,
            id,
            InteractionStatus::Expired,
            "native deadline passed",
        );
    }
    let gone: Vec<String> = gone.into_iter().chain(expired.iter().cloned()).collect();
    if !gone.is_empty() {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        for id in &gone {
            tx.m.delete(K_DEADLINE, id);
        }
        let _ = server.commit(&mut c, tx);
    }
    expired
}

// ---- interaction facts (deadline + batch equivalence) -------------------------------------------

/// The raw command can share a batch at all (one plain command).
fn batch_safe(cmd: &str) -> bool {
    const UNSAFE: &[&str] = &["#", "\n", "\r", ";", "&&", "||", "|", "`", "$("];
    !UNSAFE.iter().any(|t| cmd.contains(t))
}

/// Server facts for batch equivalence (15 §8.3), only for natively answerable approvals.
pub(super) fn approval_facts(c: &Core, i: &Interaction) -> Option<att::ApprovalFacts> {
    if i.kind != MK::Approval || i.answer_channel != AnswerChannel::Native || !i.answerable {
        return None;
    }
    let a = i.action.as_ref()?;
    let cmd = a.command.clone().unwrap_or_else(|| a.summary.clone());
    if !batch_safe(&cmd) {
        return None;
    }
    let harness = c.run(&i.run).map(|r| r.harness.clone()).unwrap_or_default();
    let pane = c.pane(&i.pane);
    let environment = pane
        .map(|p| format!("{:?}/{}", p.isolation.level, p.isolation.network))
        .unwrap_or_default();
    let policy_scope = pane
        .and_then(|p| c.ws(&p.workspace))
        .map(|w| w.root_path.clone())
        .unwrap_or_default();
    Some(att::ApprovalFacts {
        harness,
        tool: a.tool.clone(),
        normalized_command: cmd,
        policy_scope,
        environment,
        operation: a.tool.clone(),
        resource_targets: a.paths.clone(),
    })
}

/// Per open interaction: its effective deadline and batch facts (one lock).
pub(super) struct Facts {
    pub deadlines: HashMap<String, DeadlineRec>,
    pub approvals: HashMap<String, att::ApprovalFacts>,
}

pub(super) fn facts(server: &Server) -> Facts {
    server.with_core(|c| {
        let recs = deadlines(c);
        let mut out = Facts {
            deadlines: HashMap::new(),
            approvals: HashMap::new(),
        };
        for i in c
            .model
            .interactions
            .iter()
            .filter(|i| i.status == InteractionStatus::Open)
        {
            if let Some(d) = effective_deadline(&recs, i) {
                out.deadlines.insert(i.id.clone(), d.clone());
            }
            if let Some(f) = approval_facts(c, i) {
                out.approvals.insert(i.id.clone(), f);
            }
        }
        out
    })
}

/// Fill deadline and batch facts into a freshly built interaction item.
pub(super) fn decorate_item(f: &Facts, interaction: &str, it: &mut AttentionItem) {
    it.deadline_ms = f.deadlines.get(interaction).map(|d| d.deadline_ms);
    if let Some(ia) = it.interaction.as_mut() {
        ia.approval = f.approvals.get(interaction).cloned();
    }
}

// ---- attention.list extras ----------------------------------------------------------------------

/// Batches among the listed (visible, ranked) items: `{batch_id → [object ids]}`.
pub(super) fn batches(ranked: &[att::RankedItem]) -> Vec<(String, Vec<String>)> {
    let items: Vec<AttentionItem> = ranked.iter().map(|r| r.item.clone()).collect();
    att::batch_groups(&items)
        .into_iter()
        .map(|g| {
            let members: Vec<&AttentionItem> = g.iter().map(|&i| &items[i]).collect();
            (
                att::batch_id(&members),
                members.iter().map(|m| m.key.object_id.clone()).collect(),
            )
        })
        .collect()
}

/// Per-item extras for `attention.list`: deadline (with its source) and batch membership.
pub(super) fn item_extras(
    f: &Facts,
    m: &Meta,
    r: &att::RankedItem,
    batches: &[(String, Vec<String>)],
    now_ms: i64,
) -> Value {
    let dl = m.interaction.as_ref().and_then(|i| f.deadlines.get(i));
    let batch = batches
        .iter()
        .find(|(_, ms)| ms.contains(&r.item.key.object_id))
        .map(|(id, ms)| json!({"id": id, "size": ms.len()}));
    json!({
        "deadline_ms": dl.map(|d| d.deadline_ms),
        "deadline_source": dl.map(|d| d.source.clone()),
        "deadline_in_ms": dl.map(|d| (d.deadline_ms - now_ms).max(0)),
        "batch": batch,
    })
}

/// Busy runs without an open interaction (15 §8.1 **Also working** footer), scoped like the
/// list. Never ranked.
pub(super) fn also_working(server: &Server, scope: Option<&str>, now_ms: i64) -> Vec<Value> {
    if !cfg().also_working {
        return vec![];
    }
    server.with_core(|c| {
        let open: HashSet<&str> = c
            .model
            .interactions
            .iter()
            .filter(|i| i.status == InteractionStatus::Open)
            .map(|i| i.run.as_str())
            .collect();
        let bindings = tracking::bindings(c);
        let mut v: Vec<(i64, Value)> = c
            .model
            .runs
            .iter()
            .filter(|r| r.ended_at_ms.is_none() && r.execution.value == Execution::Working)
            .filter(|r| !open.contains(r.id.as_str()))
            .filter_map(|r| {
                let ws = c.pane(&r.pane).map(|p| p.workspace.clone());
                if scope.is_some_and(|s| ws.as_deref() != Some(s)) {
                    return None;
                }
                let task =
                    vk_review::binding::binding_for_turn(&bindings, &r.id, r.turns_completed + 1)
                        .filter(|b| b.state == BindingState::Active)
                        .and_then(|b| c.task(&b.task_id))
                        .map(|t| json!({"id": t.id, "handle": t.handle, "title": t.title}));
                let since = r.execution.since_ms;
                Some((
                    since,
                    json!({
                        "run": r.id,
                        "pane": r.pane,
                        "name": r.name.clone().unwrap_or_else(|| r.harness.clone()),
                        "harness": r.harness,
                        "task": task,
                        "since_ms": since,
                        "working_for_ms": (now_ms - since).max(0),
                    }),
                ))
            })
            .collect();
        v.sort_by_key(|(s, _)| *s);
        v.into_iter().map(|(_, j)| j).collect()
    })
}

/// `attention.batch {interaction}`: the batch that contains this interaction now, with each
/// member's decision revision (send it as `expected_decision_rev` to `interaction.answer`,
/// one call per member), or why it can't be batched. Read-only.
pub(super) fn batch_api(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let id = req(p, "interaction")?;
    let scope = caller_workspace(server, ctx)?;
    let now_ms = now();
    let mut col = collect(server, now_ms);
    if let Some(ws) = &scope {
        let item_ws = std::mem::take(&mut col.item_ws);
        col.items.retain(|i| {
            item_ws
                .get(&i.key.object_id)
                .is_some_and(|s| s.contains(ws))
        });
    }
    let target = server
        .with_core(|c| c.interaction(id).cloned())
        .ok_or_else(|| not_found("interaction", id))?;
    let obj = format!("interaction:{}", target.id);
    let Some(seed) = col.items.iter().find(|i| i.key.object_id == obj).cloned() else {
        return Ok(
            json!({"interaction": target.id, "batchable": false, "reason": "not an open item in your scope", "members": []}),
        );
    };
    let members = att::batch_with(&seed, &col.items);
    if members.is_empty() {
        let reason = if seed
            .interaction
            .as_ref()
            .and_then(|f| f.approval.as_ref())
            .is_none()
        {
            "answered one by one: only plain, natively answerable approvals of low or medium risk are batched"
        } else {
            "nothing equivalent is waiting"
        };
        return Ok(
            json!({"interaction": target.id, "batchable": false, "reason": reason, "members": []}),
        );
    }
    let refs: Vec<&AttentionItem> = members.to_vec();
    let bid = att::batch_id(&refs);
    let out: Vec<Value> = server.with_core(|c| {
        members
            .iter()
            .filter_map(|m| {
                let iid = m.key.object_id.strip_prefix("interaction:")?;
                let i = c.interaction(iid)?;
                Some(json!({
                    "interaction": i.id,
                    "handle": i.handle,
                    "run": i.run,
                    "pane": i.pane,
                    "title": i.title,
                    "decision_rev": i.decision_rev,
                    "opened_at_ms": i.opened_at_ms,
                }))
            })
            .collect()
    });
    let facts = seed
        .interaction
        .as_ref()
        .and_then(|f| f.approval.clone())
        .map(|a| json!({"harness": a.harness, "tool": a.tool, "command": a.normalized_command, "paths": a.resource_targets, "environment": a.environment, "policy_scope": a.policy_scope}));
    Ok(json!({
        "interaction": target.id,
        "batchable": true,
        "batch": bid,
        "members": out,
        "facts": facts,
        "note": "Answer each member with interaction.answer (expected_decision_rev); each records and delivers on its own, so a partial failure is visible per member.",
    }))
}

// ---- notifications ------------------------------------------------------------------------------

#[derive(Default)]
struct NotifyState {
    /// `object id → (revision, class)` already notified (or present at startup).
    seen: HashMap<String, (u64, u8)>,
    seeded: bool,
}

fn notify_states() -> &'static Mutex<HashMap<usize, NotifyState>> {
    static S: OnceLock<Mutex<HashMap<usize, NotifyState>>> = OnceLock::new();
    S.get_or_init(Default::default)
}

/// What to notify for a ranked item, if anything: `(kind, title, body, urgency)`.
fn notification_for(r: &att::RankedItem, m: &Meta) -> Option<(&'static str, String, String)> {
    match (r.class, m.kind) {
        (att::AttentionClass::DeliveryOrUnsafe, _) => Some((
            "attention.delivery",
            m.title.clone(),
            format!("{} · {}", m.subtitle, r.explanation),
        )),
        (att::AttentionClass::DeadlineApproaching, _) => Some((
            "attention.deadline",
            format!("Answer soon: {}", m.title),
            r.explanation.replace("; ", " · "),
        )),
        (att::AttentionClass::BlockingDecision, "check_failed") => {
            Some(("attention.review", m.title.clone(), m.subtitle.clone()))
        }
        (att::AttentionClass::ReviewCandidate, "review")
            if m.subtitle.starts_with(label_text("ready_for_review")) =>
        {
            Some(("attention.review", m.title.clone(), m.subtitle.clone()))
        }
        _ => None,
    }
}

/// One watcher pass: reconcile expired native deadlines, then notify items that became urgent
/// (or ready) since the last pass. The first pass only records what is already there.
/// Returns the notifications created.
pub fn tick(server: &Server) -> Vec<Notification> {
    let now_ms = now();
    reconcile_deadlines(server, now_ms);
    // Disposable reviewer checkouts of finished reviewers (lane 2C).
    super::scratch::sweep(server);
    let col = collect(server, now_ms);
    let ranked = att::rank(&col.items, now_ms, &prefs());
    let key = server as *const Server as usize;
    let mut todo: Vec<(&'static str, Option<String>, String, String, &'static str)> = vec![];
    {
        let mut states = notify_states().lock().unwrap();
        let st = states.entry(key).or_default();
        let seeding = !st.seeded;
        st.seeded = true;
        let mut alive = HashSet::new();
        for r in &ranked {
            let id = &r.item.key.object_id;
            alive.insert(id.clone());
            let class = r.class.number();
            let mark = (r.item.key.revision, class);
            let prev = st.seen.insert(id.clone(), mark);
            if seeding || prev == Some(mark) {
                continue;
            }
            // Only a move into a more urgent class or a new revision notifies again.
            if let Some((rev, cls)) = prev
                && rev == mark.0
                && cls <= class
            {
                continue;
            }
            let Some(m) = col.meta.get(id) else { continue };
            // Interactions notified when they opened; only their deadline is news.
            if m.kind == "interaction" && r.class != att::AttentionClass::DeadlineApproaching {
                continue;
            }
            if let Some((kind, title, mut body)) = notification_for(r, m) {
                if r.woke_from_snooze {
                    body = format!("Woke from snooze · {body}");
                }
                let urgency = if r.class == att::AttentionClass::DeliveryOrUnsafe {
                    "high"
                } else {
                    "normal"
                };
                todo.push((kind, m.pane.clone(), title, body, urgency));
            }
        }
        st.seen.retain(|k, _| alive.contains(k));
    }
    todo.into_iter()
        .map(|(kind, pane, title, body, urgency)| {
            server.notify(kind, pane.as_deref(), &title, &body, urgency)
        })
        .collect()
}

/// When the watcher must wake without a model change: the next deadline window entry, the
/// next native expiry or snooze end (capped at a minute).
fn next_wake(server: &Server, now_ms: i64) -> Duration {
    let window = cfg().deadline_window_ms;
    let mut at: Option<i64> = None;
    let mut consider = |t: i64| {
        if t > now_ms {
            at = Some(at.map_or(t, |a| a.min(t)));
        }
    };
    server.with_core(|c| {
        for d in deadlines(c).values() {
            consider(d.deadline_ms - window);
            consider(d.deadline_ms);
        }
        for p in c.store.load::<Pref>(K_PREF).unwrap_or_default() {
            if let Some(u) = p.snoozed_until_ms {
                consider(u);
            }
        }
    });
    let ms = at.map_or(60_000, |t| (t - now_ms).clamp(50, 60_000));
    Duration::from_millis(ms as u64)
}

/// Start the attention watcher once per server (from `review::recover`).
pub fn start(server: &Arc<Server>) {
    static W: OnceLock<Mutex<Vec<Weak<Server>>>> = OnceLock::new();
    let mut w = W.get_or_init(Default::default).lock().unwrap();
    w.retain(|x| x.strong_count() > 0);
    if w.iter()
        .any(|x| std::ptr::eq(x.as_ptr(), Arc::as_ptr(server)))
    {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    w.push(Arc::downgrade(server));
    let weak = Arc::downgrade(server);
    let mut rx = server.model_rev.subscribe();
    tokio::spawn(async move {
        loop {
            let Some(srv) = weak.upgrade() else { break };
            let s2 = srv.clone();
            let wake = tokio::task::spawn_blocking(move || {
                tick(&s2);
                next_wake(&s2, now())
            })
            .await
            .unwrap_or(Duration::from_secs(60));
            drop(srv);
            tokio::select! {
                r = rx.changed() => if r.is_err() { break },
                _ = tokio::time::sleep(wake) => {}
            }
            // Coalesce bursts of commits.
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn payload_deadlines() {
        let now = 1_000_000;
        assert_eq!(
            deadline_from_payload(&json!({"deadline_ms": 5}), now),
            Some(5)
        );
        assert_eq!(
            deadline_from_payload(&json!({"timeout_ms": 30_000}), now),
            Some(now + 30_000)
        );
        assert_eq!(
            deadline_from_payload(&json!({"timeout": 45}), now),
            Some(now + 45_000)
        );
        assert_eq!(deadline_from_payload(&json!({"timeout": 0}), now), None);
        assert_eq!(deadline_from_payload(&json!({}), now), None);
    }

    #[test]
    fn gate_deadlines_only_count_while_the_gate_is_held() {
        let mut recs = HashMap::new();
        recs.insert(
            "i1".to_string(),
            DeadlineRec {
                interaction: "i1".into(),
                deadline_ms: 10,
                source: "gate".into(),
                recorded_at_ms: 0,
            },
        );
        let mut i = crate::agents::harness_tests_blank();
        i.id = "i1".into();
        i.gate = true;
        assert!(effective_deadline(&recs, &i).is_some());
        i.gate = false;
        assert!(effective_deadline(&recs, &i).is_none());
        recs.get_mut("i1").unwrap().source = "native".into();
        assert!(effective_deadline(&recs, &i).is_some());
    }

    #[test]
    fn compound_commands_never_batch() {
        assert!(batch_safe("cargo test"));
        for c in ["a; b", "a && b", "a | b", "a # b", "$(x)", "`x`", "a\nb"] {
            assert!(!batch_safe(c), "{c}");
        }
    }
}
