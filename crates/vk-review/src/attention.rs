//! T3 attention inbox (§8): deterministic ranking with explanations, the five-minute view,
//! snooze wake-ups, stable selection and conservative batching.
//!
//! Classes (§8.1), in order:
//! 1. Unconfirmed/failed decision delivery, or source errors that make continued action
//!    unsafe/uncertain.
//! 2. Open interactions whose native deadline is within the window (default 60 s).
//! 3. Other blocking decisions (interactions, ordinary actionable errors): explicit priority,
//!    run-blocking, then waiting time.
//! 4. Review candidates: priority, unseen before seen, then age.
//!
//! Unseen finished turns follow as the **Finished turns** footer (the M1 `next_attention`
//! fallback). Within a class: pinned first; risk raises prominence (never implying approval is
//! recommended); age and object id are stable tie-breakers.

use std::cmp::Ordering;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionKind {
    DeliveryProblem,
    Interaction,
    ReviewCandidate,
    Error,
    FinishedTurn,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Quick,
    FewMinutes,
    DeepReview,
    #[default]
    Unknown,
}

impl Effort {
    /// Coarse cost used by the five-minute view. Unknown is eligible and assumed "a few
    /// minutes"; these are not estimates of finish time.
    pub fn nominal_ms(self) -> i64 {
        match self {
            Effort::Quick => 60_000,
            Effort::FewMinutes => 180_000,
            Effort::DeepReview => 900_000,
            Effort::Unknown => 180_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Risk {
    Low,
    Medium,
    High,
    Unknown,
}

fn risk_rank(r: Option<Risk>) -> u8 {
    match r {
        None | Some(Risk::Low) => 0,
        Some(Risk::Medium) => 1,
        Some(Risk::Unknown) => 2,
        Some(Risk::High) => 3,
    }
}

/// Stable object id plus its material revision. Sources bump `revision` only for material
/// changes (decision revision, candidate subject, delivery state) — never for log output.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ItemKey {
    pub object_id: String,
    pub revision: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InteractionKind {
    Approval,
    Question,
    PlanReview,
    Notice,
}

/// What makes two native approvals equivalent for batching (§8.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalFacts {
    pub harness: String,
    pub tool: String,
    pub normalized_command: String,
    pub policy_scope: String,
    pub environment: String,
    pub operation: String,
    #[serde(default)]
    pub resource_targets: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InteractionFacts {
    pub kind: InteractionKind,
    /// The decision is still live at the source (not expired/resolved).
    pub live: bool,
    /// Answerable through a tested native capability.
    pub native_answer: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval: Option<ApprovalFacts>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    pub key: ItemKey,
    pub kind: AttentionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    pub machine: String,
    pub title: String,
    pub opened_at_ms: i64,
    /// Native deadline of an interaction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<i64>,
    /// Explicit user task priority (higher first; 0 = unset).
    #[serde(default)]
    pub priority: i32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<Risk>,
    #[serde(default)]
    pub blocks_run: bool,
    /// Open tasks waiting for this item's task through user-confirmed `blocks` dependency
    /// edges (T4, §8.1). Inferred links never count.
    #[serde(default)]
    pub blocks_tasks: u32,
    #[serde(default)]
    pub effort: Effort,
    #[serde(default)]
    pub seen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub snoozed_until_ms: Option<i64>,
    #[serde(default)]
    pub pinned: bool,
    /// For `error` items: the error makes continued action unsafe or uncertain (storage
    /// unavailable, lost verification runner with uncertain outcome, lost identity during a
    /// pending send). Ordinary setup/test failures leave this false.
    #[serde(default)]
    pub unsafe_source: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction: Option<InteractionFacts>,
}

impl AttentionItem {
    fn is_snoozed(&self, now_ms: i64) -> bool {
        self.snoozed_until_ms.is_some_and(|t| t > now_ms)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionClass {
    DeliveryOrUnsafe,
    DeadlineApproaching,
    BlockingDecision,
    ReviewCandidate,
    FinishedTurns,
}

impl AttentionClass {
    /// 1–4 per §8.1; 5 for the finished-turns footer.
    pub fn number(self) -> u8 {
        self as u8 + 1
    }
    pub fn is_urgent(self) -> bool {
        matches!(
            self,
            AttentionClass::DeliveryOrUnsafe | AttentionClass::DeadlineApproaching
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionPrefs {
    /// An interaction is class 2 when at most this much time remains before its deadline.
    pub deadline_window_ms: i64,
}

impl Default for AttentionPrefs {
    fn default() -> Self {
        AttentionPrefs {
            deadline_window_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RankedItem {
    pub item: AttentionItem,
    pub class: AttentionClass,
    /// Built from available data only, e.g. "Waiting 12m; blocks this run".
    pub explanation: String,
    /// Shown despite an active snooze because it became urgent.
    #[serde(default)]
    pub woke_from_snooze: bool,
}

/// Compact human duration: `45s`, `12m`, `1h 5m`, `2d`.
pub fn format_duration(ms: i64) -> String {
    let s = (ms.max(0) + 500) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else if s < 86_400 {
        let (h, m) = (s / 3600, (s % 3600) / 60);
        if m == 0 {
            format!("{h}h")
        } else {
            format!("{h}h {m}m")
        }
    } else {
        format!("{}d", s / 86_400)
    }
}

fn classify(item: &AttentionItem, now_ms: i64, prefs: &AttentionPrefs) -> Option<AttentionClass> {
    match item.kind {
        AttentionKind::DeliveryProblem => Some(AttentionClass::DeliveryOrUnsafe),
        AttentionKind::Error if item.unsafe_source => Some(AttentionClass::DeliveryOrUnsafe),
        AttentionKind::Error => Some(AttentionClass::BlockingDecision),
        AttentionKind::Interaction => match item.deadline_ms {
            // Expired native requests are reconciled at the source, not left answerable.
            Some(d) if d <= now_ms => None,
            Some(d) if d - now_ms <= prefs.deadline_window_ms => {
                Some(AttentionClass::DeadlineApproaching)
            }
            _ => Some(AttentionClass::BlockingDecision),
        },
        AttentionKind::ReviewCandidate => Some(AttentionClass::ReviewCandidate),
        AttentionKind::FinishedTurn if item.seen => None,
        AttentionKind::FinishedTurn => Some(AttentionClass::FinishedTurns),
    }
}

fn explain(item: &AttentionItem, class: AttentionClass, now_ms: i64, woke: bool) -> String {
    let mut parts: Vec<String> = Vec::new();
    match (item.kind, class) {
        (AttentionKind::DeliveryProblem, _) => parts.push("delivery not confirmed".into()),
        (AttentionKind::Error, AttentionClass::DeliveryOrUnsafe) => {
            parts.push("unsafe to continue until resolved".into())
        }
        _ => {}
    }
    if let Some(d) = item.deadline_ms {
        parts.push(format!("deadline in {}", format_duration(d - now_ms)));
    }
    match item.kind {
        AttentionKind::ReviewCandidate => parts.push(format!(
            "candidate {} old",
            format_duration(now_ms - item.opened_at_ms)
        )),
        AttentionKind::FinishedTurn => parts.push(format!(
            "finished {} ago",
            format_duration(now_ms - item.opened_at_ms)
        )),
        _ => parts.push(format!(
            "waiting {}",
            format_duration(now_ms - item.opened_at_ms)
        )),
    }
    if item.blocks_run {
        parts.push("blocks this run".into());
    }
    match item.blocks_tasks {
        0 => {}
        1 => parts.push("blocks 1 linked task".into()),
        n => parts.push(format!("blocks {n} linked tasks")),
    }
    if item.priority != 0 {
        parts.push(format!("priority {:+}", item.priority));
    }
    match item.risk {
        Some(Risk::High) => parts.push("high risk".into()),
        Some(Risk::Unknown) => parts.push("risk unknown".into()),
        _ => {}
    }
    if item.kind == AttentionKind::ReviewCandidate && !item.seen {
        parts.push("not yet seen".into());
    }
    if item.pinned {
        parts.push("pinned".into());
    }
    if woke {
        parts.push("woke from snooze".into());
    }
    let mut s = parts.join("; ");
    if let Some(first) = s.get(..1) {
        let up = first.to_uppercase();
        s.replace_range(..1, &up);
    }
    s
}

fn within_class(a: &RankedItem, b: &RankedItem) -> Ordering {
    let (x, y) = (&a.item, &b.item);
    let by_pin = y.pinned.cmp(&x.pinned);
    let by_age = x
        .opened_at_ms
        .cmp(&y.opened_at_ms)
        .then_with(|| x.key.object_id.cmp(&y.key.object_id));
    let by_risk = risk_rank(y.risk).cmp(&risk_rank(x.risk));
    // Explicit priority, then confirmed dependent tasks (§8.1 class 3).
    let by_prio = y
        .priority
        .cmp(&x.priority)
        .then_with(|| y.blocks_tasks.cmp(&x.blocks_tasks));
    match a.class {
        AttentionClass::DeliveryOrUnsafe => by_pin.then(by_risk).then(by_age),
        AttentionClass::DeadlineApproaching => by_pin
            .then_with(|| x.deadline_ms.cmp(&y.deadline_ms))
            .then(by_risk)
            .then(by_age),
        AttentionClass::BlockingDecision => by_pin
            .then(by_prio)
            .then_with(|| y.blocks_run.cmp(&x.blocks_run))
            .then(by_risk)
            .then(by_age),
        AttentionClass::ReviewCandidate => by_pin
            .then(by_prio)
            .then_with(|| x.seen.cmp(&y.seen))
            .then(by_risk)
            .then(by_age),
        AttentionClass::FinishedTurns => by_age,
    }
}

/// Rank attention items (§8.1). Snoozed items are omitted unless they became urgent (class 1
/// or 2); expired interactions and seen finished turns are omitted.
pub fn rank(items: &[AttentionItem], now_ms: i64, prefs: &AttentionPrefs) -> Vec<RankedItem> {
    let mut out: Vec<RankedItem> = items
        .iter()
        .filter_map(|it| {
            let class = classify(it, now_ms, prefs)?;
            let woke = it.is_snoozed(now_ms);
            if woke && !class.is_urgent() {
                return None;
            }
            Some(RankedItem {
                explanation: explain(it, class, now_ms, woke),
                item: it.clone(),
                class,
                woke_from_snooze: woke,
            })
        })
        .collect();
    out.sort_by(|a, b| a.class.cmp(&b.class).then_with(|| within_class(a, b)));
    out
}

/// What `prefix+a` opens: the first ranked item (actionable first, then the oldest unseen
/// finished turn).
pub fn next_attention(ranked: &[RankedItem]) -> Option<&RankedItem> {
    ranked.first()
}

pub const URGENT_NOTE: &str = "Urgent — may take longer";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShortlistEntry {
    pub ranked: RankedItem,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FiveMinuteView {
    /// In rank order.
    pub items: Vec<ShortlistEntry>,
    /// Actionable items not in the shortlist (still in **All items**).
    pub omitted_count: usize,
    /// All actionable items.
    pub total_count: usize,
    /// Sum of coarse effort values of the shortlist.
    pub nominal_ms: i64,
    pub note: String,
}

/// Suggested working set for `budget_ms` (§8.2). Urgent items (classes 1–2) are always included;
/// other candidates prefer run-blocking items and lower coarse effort, unknown effort eligible.
/// Finished turns are not part of the shortlist.
pub fn five_minute_view(ranked: &[RankedItem], budget_ms: i64) -> FiveMinuteView {
    let actionable: Vec<(usize, &RankedItem)> = ranked
        .iter()
        .enumerate()
        .filter(|(_, r)| r.class != AttentionClass::FinishedTurns)
        .collect();
    let mut chosen: Vec<(usize, Option<String>)> = Vec::new();
    let mut used = 0i64;
    for (i, r) in actionable.iter().filter(|(_, r)| r.class.is_urgent()) {
        let cost = r.item.effort.nominal_ms();
        used += cost;
        let long =
            used > budget_ms || matches!(r.item.effort, Effort::DeepReview | Effort::Unknown);
        chosen.push((*i, long.then(|| URGENT_NOTE.to_string())));
    }
    let mut rest: Vec<&(usize, &RankedItem)> = actionable
        .iter()
        .filter(|(_, r)| !r.class.is_urgent())
        .collect();
    rest.sort_by(|(ia, a), (ib, b)| {
        b.item
            .blocks_run
            .cmp(&a.item.blocks_run)
            .then_with(|| b.item.blocks_tasks.cmp(&a.item.blocks_tasks))
            .then_with(|| a.item.effort.nominal_ms().cmp(&b.item.effort.nominal_ms()))
            .then(ia.cmp(ib))
    });
    for (i, r) in rest {
        let cost = r.item.effort.nominal_ms();
        if used + cost <= budget_ms {
            used += cost;
            chosen.push((*i, None));
        }
    }
    chosen.sort_by_key(|(i, _)| *i);
    let total = actionable.len();
    let omitted = total - chosen.len();
    let mut note = format!(
        "Suggested set from coarse effort labels; it may take longer than {}.",
        format_duration(budget_ms)
    );
    if omitted > 0 {
        note.push_str(&format!(" {omitted} more in All items."));
    }
    FiveMinuteView {
        items: chosen
            .into_iter()
            .map(|(i, note)| ShortlistEntry {
                ranked: ranked[i].clone(),
                note,
            })
            .collect(),
        omitted_count: omitted,
        total_count: total,
        nominal_ms: used,
        note,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum WakeReason {
    DeadlineApproaching {
        deadline_ms: i64,
    },
    DeliveryUncertainty,
    RiskEscalation {
        from: Option<Risk>,
        to: Option<Risk>,
    },
    MaterialRevision {
        from: u64,
        to: u64,
    },
}

impl WakeReason {
    pub fn text(&self, now_ms: i64) -> String {
        match self {
            WakeReason::DeadlineApproaching { deadline_ms } => {
                format!("Deadline in {}", format_duration(deadline_ms - now_ms))
            }
            WakeReason::DeliveryUncertainty => "Delivery became uncertain".into(),
            WakeReason::RiskEscalation { .. } => "Risk increased".into(),
            WakeReason::MaterialRevision { .. } => "Changed since you snoozed it".into(),
        }
    }
}

/// Whether a snoozed item should wake (§8.3). `prev` is the item as it was when snoozed (same
/// object). Unrelated changes (no revision bump, same risk/kind/deadline) never wake it.
pub fn wake(
    item: &AttentionItem,
    prev: &AttentionItem,
    now_ms: i64,
    prefs: &AttentionPrefs,
) -> Option<WakeReason> {
    if !item.is_snoozed(now_ms) || item.key.object_id != prev.key.object_id {
        return None;
    }
    if let Some(d) = item.deadline_ms
        && d > now_ms
        && d - now_ms <= prefs.deadline_window_ms
    {
        return Some(WakeReason::DeadlineApproaching { deadline_ms: d });
    }
    let uncertain = |i: &AttentionItem| {
        i.kind == AttentionKind::DeliveryProblem
            || (i.kind == AttentionKind::Error && i.unsafe_source)
    };
    if uncertain(item) && !uncertain(prev) {
        return Some(WakeReason::DeliveryUncertainty);
    }
    if risk_rank(item.risk) > risk_rank(prev.risk) {
        return Some(WakeReason::RiskEscalation {
            from: prev.risk,
            to: item.risk,
        });
    }
    if item.key.revision > prev.key.revision {
        return Some(WakeReason::MaterialRevision {
            from: prev.key.revision,
            to: item.key.revision,
        });
    }
    None
}

/// Shown before accepting a snooze: the harness may time out first.
pub fn snooze_warning(item: &AttentionItem, until_ms: i64, now_ms: i64) -> Option<String> {
    let d = item.deadline_ms?;
    (d < until_ms).then(|| {
        format!(
            "The agent's request expires in {} — before this snooze ends",
            format_duration(d - now_ms)
        )
    })
}

/// Index of the previously selected object in the re-ranked list (matched by object id, so a
/// new revision keeps the selection). If it disappeared, select the top item.
pub fn stable_selection(
    prev_selected: Option<&ItemKey>,
    new_ranked: &[RankedItem],
) -> Option<usize> {
    if new_ranked.is_empty() {
        return None;
    }
    prev_selected
        .and_then(|k| {
            new_ranked
                .iter()
                .position(|r| r.item.key.object_id == k.object_id)
        })
        .or(Some(0))
}

/// Items that became urgent since `prev` (for an indicator instead of moving the selection).
pub fn newly_urgent(prev: &[RankedItem], new: &[RankedItem]) -> Vec<ItemKey> {
    new.iter()
        .filter(|r| r.class.is_urgent())
        .filter(|r| {
            !prev
                .iter()
                .any(|p| p.class.is_urgent() && p.item.key.object_id == r.item.key.object_id)
        })
        .map(|r| r.item.key.clone())
        .collect()
}

/// Whether two items may be answered as one batch (§8.3): both live, natively answerable
/// approvals on the same machine with equivalent harness, tool, normalized command, policy
/// scope, environment, operation and resource targets, each with known risk ≤ medium. Never
/// questions, plan reviews, notices, high/unknown risk, reviews or the same object twice.
pub fn batchable(a: &AttentionItem, b: &AttentionItem) -> bool {
    fn ok(i: &AttentionItem) -> Option<&ApprovalFacts> {
        if i.kind != AttentionKind::Interaction || !matches!(i.risk, Some(Risk::Low | Risk::Medium))
        {
            return None;
        }
        let f = i.interaction.as_ref()?;
        if f.kind != InteractionKind::Approval || !f.live || !f.native_answer {
            return None;
        }
        f.approval.as_ref()
    }
    let (Some(x), Some(y)) = (ok(a), ok(b)) else {
        return false;
    };
    if a.key.object_id == b.key.object_id || a.machine != b.machine {
        return false;
    }
    // A compound command (comment, line break, `;`, `&&`, `||`, pipe, backticks, `$(`) is
    // never batched: what a reviewer sees of one says nothing reliable about another.
    const UNSAFE: &[&str] = &["#", "\n", "\r", ";", "&&", "||", "|", "`", "$("];
    if [x, y]
        .iter()
        .any(|f| UNSAFE.iter().any(|t| f.normalized_command.contains(t)))
    {
        return false;
    }
    let mut tx = x.resource_targets.clone();
    let mut ty = y.resource_targets.clone();
    tx.sort();
    tx.dedup();
    ty.sort();
    ty.dedup();
    x.harness == y.harness
        && x.tool == y.tool
        && x.normalized_command == y.normalized_command
        && x.policy_scope == y.policy_scope
        && x.environment == y.environment
        && x.operation == y.operation
        && tx == ty
}

/// Members batchable with `seed` (including `seed` itself first), in input order.
pub fn batch_with<'a>(
    seed: &'a AttentionItem,
    items: &'a [AttentionItem],
) -> Vec<&'a AttentionItem> {
    let mut out = vec![seed];
    out.extend(items.iter().filter(|i| batchable(seed, i)));
    if out.len() == 1 { vec![] } else { out }
}

/// Partition `items` into batches (§8.3): each group holds indices of items pairwise
/// [`batchable`] with its first member (in input order), with at least two members; an item
/// belongs to at most one group. Items in no group are answered one by one.
pub fn batch_groups(items: &[AttentionItem]) -> Vec<Vec<usize>> {
    let mut taken = vec![false; items.len()];
    let mut out = Vec::new();
    for i in 0..items.len() {
        if taken[i] {
            continue;
        }
        let mut group = vec![i];
        for j in (i + 1)..items.len() {
            if !taken[j] && group.iter().all(|&g| batchable(&items[g], &items[j])) {
                group.push(j);
            }
        }
        if group.len() > 1 {
            for &g in &group {
                taken[g] = true;
            }
            out.push(group);
        }
    }
    out
}

/// A batch's stable id: the sorted object ids of its members, hashed (so clients can refer to
/// "this batch" and notice when its membership changed).
pub fn batch_id(members: &[&AttentionItem]) -> String {
    let mut ids: Vec<&str> = members.iter().map(|m| m.key.object_id.as_str()).collect();
    ids.sort();
    let mut h = crate::FieldHasher::new("vk-review/attention-batch/v1");
    for id in ids {
        h.str(id);
    }
    format!("b_{}", &h.finish()[..16])
}

/// Whether an interaction's native deadline has passed (§8.1: expired native requests are
/// reconciled and do not remain answerable because their card is cached).
pub fn expired(item: &AttentionItem, now_ms: i64) -> bool {
    item.kind == AttentionKind::Interaction && item.deadline_ms.is_some_and(|d| d <= now_ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 10_000_000;
    const MIN: i64 = 60_000;

    fn item(id: &str, kind: AttentionKind, opened_ago_ms: i64) -> AttentionItem {
        AttentionItem {
            key: ItemKey {
                object_id: id.into(),
                revision: 1,
            },
            kind,
            task_id: Some("t".into()),
            run_id: Some("r".into()),
            machine: "laptop".into(),
            title: id.into(),
            opened_at_ms: NOW - opened_ago_ms,
            deadline_ms: None,
            priority: 0,
            risk: None,
            blocks_run: false,
            blocks_tasks: 0,
            effort: Effort::Unknown,
            seen: false,
            snoozed_until_ms: None,
            pinned: false,
            unsafe_source: false,
            interaction: None,
        }
    }

    fn ids(r: &[RankedItem]) -> Vec<&str> {
        r.iter().map(|x| x.item.key.object_id.as_str()).collect()
    }

    fn p() -> AttentionPrefs {
        AttentionPrefs::default()
    }

    #[test]
    fn class_order() {
        let mut deadline = item("deadline", AttentionKind::Interaction, MIN);
        deadline.deadline_ms = Some(NOW + 45_000);
        let mut far = item("far-deadline", AttentionKind::Interaction, 2 * MIN);
        far.deadline_ms = Some(NOW + 10 * MIN);
        let mut unsafe_err = item("storage", AttentionKind::Error, 0);
        unsafe_err.unsafe_source = true;
        let items = vec![
            item("done", AttentionKind::FinishedTurn, 50 * MIN),
            item("review", AttentionKind::ReviewCandidate, 60 * MIN),
            item("test-fail", AttentionKind::Error, 30 * MIN),
            far,
            deadline,
            item("delivery", AttentionKind::DeliveryProblem, 0),
            unsafe_err,
        ];
        let r = rank(&items, NOW, &p());
        assert_eq!(
            ids(&r),
            vec![
                "delivery",
                "storage",
                "deadline",
                "test-fail",
                "far-deadline",
                "review",
                "done"
            ]
        );
        assert_eq!(r[0].class.number(), 1);
        assert_eq!(r[2].class.number(), 2);
        assert_eq!(r[3].class.number(), 3);
        assert_eq!(r[5].class.number(), 4);
        assert_eq!(r[6].class, AttentionClass::FinishedTurns);
        assert_eq!(r[2].explanation, "Deadline in 45s; waiting 1m");
        assert_eq!(next_attention(&r).unwrap().item.key.object_id, "delivery");
    }

    #[test]
    fn ordinary_failures_do_not_outrank_expiring_approvals() {
        let mut approval = item("approval", AttentionKind::Interaction, 0);
        approval.deadline_ms = Some(NOW + 30_000);
        let mut err = item("setup-failed", AttentionKind::Error, 99 * MIN);
        err.blocks_run = true;
        err.priority = 9;
        let r = rank(&[err, approval], NOW, &p());
        assert_eq!(ids(&r), vec!["approval", "setup-failed"]);
    }

    #[test]
    fn class3_priority_blocking_risk_age() {
        let mut a = item("a", AttentionKind::Interaction, 12 * MIN);
        a.blocks_run = true;
        let b = item("b", AttentionKind::Interaction, 30 * MIN);
        let mut c = item("c", AttentionKind::Interaction, MIN);
        c.priority = 2;
        let mut d = item("d", AttentionKind::Interaction, 5 * MIN);
        d.risk = Some(Risk::High);
        let mut e = item("e", AttentionKind::Interaction, 0);
        e.pinned = true;
        let r = rank(&[a, b, c, d, e], NOW, &p());
        assert_eq!(ids(&r), vec!["e", "c", "a", "d", "b"]);
        assert_eq!(r[2].explanation, "Waiting 12m; blocks this run");
        assert!(r[3].explanation.contains("high risk"));
        assert!(!r[3].explanation.to_lowercase().contains("recommend"));
    }

    #[test]
    fn confirmed_dependents_rank_after_priority_and_explain() {
        let old = item("old", AttentionKind::ReviewCandidate, 30 * MIN);
        let mut blocking = item("blocking", AttentionKind::ReviewCandidate, MIN);
        blocking.blocks_tasks = 2;
        let mut one = item("one", AttentionKind::Interaction, MIN);
        one.blocks_tasks = 1;
        let mut prio = item("prio", AttentionKind::ReviewCandidate, 0);
        prio.priority = 1;
        let r = rank(&[old, blocking, one, prio], NOW, &p());
        assert_eq!(ids(&r), vec!["one", "prio", "blocking", "old"]);
        assert_eq!(r[0].explanation, "Waiting 1m; blocks 1 linked task");
        assert!(r[2].explanation.contains("blocks 2 linked tasks"));
        assert!(!r[3].explanation.contains("blocks"));
        // The five-minute view prefers unblock impact among equal effort.
        let v = five_minute_view(&r, 60_000 * 3);
        assert_eq!(v.items.len(), 1);
        assert_eq!(v.items[0].ranked.item.key.object_id, "blocking");
    }

    #[test]
    fn risk_only_raises_within_class() {
        let mut risky_review = item("review", AttentionKind::ReviewCandidate, 0);
        risky_review.risk = Some(Risk::High);
        risky_review.pinned = true;
        let q = item("q", AttentionKind::Interaction, 0);
        let r = rank(&[risky_review, q], NOW, &p());
        assert_eq!(ids(&r), vec!["q", "review"]);
    }

    #[test]
    fn reviews_unseen_before_seen_and_age_tiebreak() {
        let mut seen = item("seen", AttentionKind::ReviewCandidate, 90 * MIN);
        seen.seen = true;
        let unseen_new = item("unseen-new", AttentionKind::ReviewCandidate, MIN);
        let unseen_old = item("unseen-old", AttentionKind::ReviewCandidate, 10 * MIN);
        let r = rank(&[seen, unseen_new, unseen_old], NOW, &p());
        assert_eq!(ids(&r), vec!["unseen-old", "unseen-new", "seen"]);
    }

    #[test]
    fn expired_and_seen_finished_omitted_and_finished_fallback() {
        let mut expired = item("expired", AttentionKind::Interaction, MIN);
        expired.deadline_ms = Some(NOW - 1);
        let mut seen_done = item("seen-done", AttentionKind::FinishedTurn, 5 * MIN);
        seen_done.seen = true;
        let old_done = item("old-done", AttentionKind::FinishedTurn, 50 * MIN);
        let new_done = item("new-done", AttentionKind::FinishedTurn, 5 * MIN);
        let r = rank(&[expired, seen_done, new_done, old_done], NOW, &p());
        // §12: no tracked tasks and no pending interactions → oldest unseen done run.
        assert_eq!(ids(&r), vec!["old-done", "new-done"]);
        assert_eq!(next_attention(&r).unwrap().item.key.object_id, "old-done");
    }

    #[test]
    fn urgent_item_survives_five_minute_view() {
        // §12: five-minute view with a large urgent decision.
        let mut urgent = item("urgent", AttentionKind::DeliveryProblem, 0);
        urgent.effort = Effort::DeepReview;
        let mut quick = item("quick", AttentionKind::ReviewCandidate, 3 * MIN);
        quick.effort = Effort::Quick;
        let mut deep = item("deep", AttentionKind::ReviewCandidate, 99 * MIN);
        deep.effort = Effort::DeepReview;
        let mut unknown = item("unknown", AttentionKind::Interaction, MIN);
        unknown.effort = Effort::Unknown;
        let r = rank(&[deep, quick, unknown, urgent], NOW, &p());
        let v = five_minute_view(&r, 5 * MIN);
        let got: Vec<&str> = v
            .items
            .iter()
            .map(|e| e.ranked.item.key.object_id.as_str())
            .collect();
        assert_eq!(got[0], "urgent");
        assert_eq!(v.items[0].note.as_deref(), Some(URGENT_NOTE));
        assert_eq!(v.total_count, 4);
        assert!(v.omitted_count >= 1);
        assert!(got.contains(&"urgent"));
        assert!(!got.contains(&"deep"));
        assert!(v.note.contains("may take longer"));
        assert!(v.note.contains("more in All items"));

        // Without urgent items: prefers blocking and low effort; unknown effort eligible.
        let mut blocking = item("blocking", AttentionKind::Interaction, 0);
        blocking.blocks_run = true;
        blocking.effort = Effort::FewMinutes;
        let mut q1 = item("q1", AttentionKind::ReviewCandidate, MIN);
        q1.effort = Effort::Quick;
        let u = item("u", AttentionKind::ReviewCandidate, 2 * MIN);
        let r = rank(&[u, q1, blocking], NOW, &p());
        let v = five_minute_view(&r, 5 * MIN);
        let got: Vec<&str> = v
            .items
            .iter()
            .map(|e| e.ranked.item.key.object_id.as_str())
            .collect();
        assert_eq!(got, vec!["blocking", "q1"]);
        assert_eq!(v.omitted_count, 1);
        let r2 = rank(&[item("u", AttentionKind::ReviewCandidate, 0)], NOW, &p());
        assert_eq!(five_minute_view(&r2, 5 * MIN).items.len(), 1);
    }

    #[test]
    fn snooze_hides_until_urgent_and_wakes_on_deadline() {
        // §12: snooze meets deadline.
        let mut q = item("q", AttentionKind::Interaction, MIN);
        q.snoozed_until_ms = Some(NOW + 60 * MIN);
        q.deadline_ms = Some(NOW + 10 * MIN);
        let prev = q.clone();
        assert!(rank(std::slice::from_ref(&q), NOW, &p()).is_empty());
        assert_eq!(wake(&q, &prev, NOW, &p()), None);
        assert!(
            snooze_warning(&q, NOW + 60 * MIN, NOW)
                .unwrap()
                .contains("10m")
        );

        let later = NOW + 9 * MIN + 30_000;
        let w = wake(&q, &prev, later, &p()).unwrap();
        assert_eq!(
            w,
            WakeReason::DeadlineApproaching {
                deadline_ms: NOW + 10 * MIN
            }
        );
        assert_eq!(w.text(later), "Deadline in 30s");
        let r = rank(std::slice::from_ref(&q), later, &p());
        assert_eq!(r.len(), 1);
        assert!(r[0].woke_from_snooze);
        assert_eq!(r[0].class, AttentionClass::DeadlineApproaching);
        // Snooze elapsed: back in normal ranking.
        assert_eq!(
            rank(&[q], NOW + 61 * MIN, &p()).len(),
            0,
            "expired deadline omitted"
        );
    }

    #[test]
    fn wake_reasons_material_only() {
        let mut base = item("x", AttentionKind::ReviewCandidate, MIN);
        base.snoozed_until_ms = Some(NOW + 60 * MIN);
        base.risk = Some(Risk::Low);
        // Unrelated change (title/log) without revision bump → no wake.
        let mut same = base.clone();
        same.title = "new log line".into();
        assert_eq!(wake(&same, &base, NOW, &p()), None);
        let mut risk = base.clone();
        risk.risk = Some(Risk::High);
        assert!(matches!(
            wake(&risk, &base, NOW, &p()),
            Some(WakeReason::RiskEscalation { .. })
        ));
        let mut rev = base.clone();
        rev.key.revision = 2;
        assert_eq!(
            wake(&rev, &base, NOW, &p()),
            Some(WakeReason::MaterialRevision { from: 1, to: 2 })
        );
        let mut delivery = base.clone();
        delivery.kind = AttentionKind::DeliveryProblem;
        assert_eq!(
            wake(&delivery, &base, NOW, &p()),
            Some(WakeReason::DeliveryUncertainty)
        );
        // Not snoozed → nothing to wake.
        let mut awake = rev;
        awake.snoozed_until_ms = None;
        assert_eq!(wake(&awake, &base, NOW, &p()), None);
    }

    #[test]
    fn stable_selection_while_reordering() {
        // §12: inbox reorders while typing.
        let a = item("a", AttentionKind::Interaction, 10 * MIN);
        let b = item("b", AttentionKind::Interaction, 5 * MIN);
        let r1 = rank(&[a.clone(), b.clone()], NOW, &p());
        let sel = stable_selection(None, &r1).unwrap();
        assert_eq!(r1[sel].item.key.object_id, "a");
        let selected = r1[sel].item.key.clone();
        // A new urgent item arrives above and `a` gets a new revision.
        let mut urgent = item("u", AttentionKind::DeliveryProblem, 0);
        urgent.title = "urgent".into();
        let mut a2 = a;
        a2.key.revision = 2;
        let r2 = rank(&[a2, b, urgent], NOW, &p());
        let sel2 = stable_selection(Some(&selected), &r2).unwrap();
        assert_eq!(r2[sel2].item.key.object_id, "a");
        assert_eq!(sel2, 1);
        assert_eq!(
            newly_urgent(&r1, &r2),
            vec![ItemKey {
                object_id: "u".into(),
                revision: 1
            }]
        );
        // Selected object resolved → top.
        let r3 = rank(&[item("b", AttentionKind::Interaction, 0)], NOW, &p());
        assert_eq!(stable_selection(Some(&selected), &r3), Some(0));
        assert_eq!(stable_selection(Some(&selected), &[]), None);
    }

    fn approval(id: &str, cmd: &str) -> AttentionItem {
        let mut i = item(id, AttentionKind::Interaction, 0);
        i.risk = Some(Risk::Low);
        i.interaction = Some(InteractionFacts {
            kind: InteractionKind::Approval,
            live: true,
            native_answer: true,
            approval: Some(ApprovalFacts {
                harness: "claude".into(),
                tool: "Bash".into(),
                normalized_command: cmd.into(),
                policy_scope: "session".into(),
                environment: "host".into(),
                operation: "exec".into(),
                resource_targets: vec!["/r".into()],
            }),
        });
        i
    }

    #[test]
    fn batch_never_includes_questions_or_risky_items() {
        let a = approval("a", "cargo test");
        let b = approval("b", "cargo test");
        assert!(batchable(&a, &b));
        assert!(!batchable(&a, &a), "same object");
        assert!(!batchable(&a, &approval("c", "cargo build")));
        // Compound commands never batch, not even with an identical twin.
        for cmd in [
            "echo a # rm x",
            "echo a #\nrm x",
            "a; b",
            "a && b",
            "a | b",
            "a $(b)",
        ] {
            assert!(
                !batchable(&approval("x", cmd), &approval("y", cmd)),
                "{cmd:?}"
            );
        }

        let mut q = approval("q", "cargo test");
        q.interaction.as_mut().unwrap().kind = InteractionKind::Question;
        assert!(!batchable(&a, &q));
        let mut plan = approval("p", "cargo test");
        plan.interaction.as_mut().unwrap().kind = InteractionKind::PlanReview;
        assert!(!batchable(&a, &plan));
        for risk in [Some(Risk::High), Some(Risk::Unknown), None] {
            let mut r = approval("r", "cargo test");
            r.risk = risk;
            assert!(!batchable(&a, &r), "{risk:?}");
        }
        let mut medium = approval("m", "cargo test");
        medium.risk = Some(Risk::Medium);
        assert!(batchable(&a, &medium));
        let mut dead = approval("d", "cargo test");
        dead.interaction.as_mut().unwrap().live = false;
        assert!(!batchable(&a, &dead));
        let mut keystrokes = approval("k", "cargo test");
        keystrokes.interaction.as_mut().unwrap().native_answer = false;
        assert!(!batchable(&a, &keystrokes));
        let mut scope = approval("s", "cargo test");
        scope
            .interaction
            .as_mut()
            .unwrap()
            .approval
            .as_mut()
            .unwrap()
            .policy_scope = "always".into();
        assert!(!batchable(&a, &scope));
        let mut other_harness = approval("h", "cargo test");
        other_harness
            .interaction
            .as_mut()
            .unwrap()
            .approval
            .as_mut()
            .unwrap()
            .harness = "codex".into();
        assert!(!batchable(&a, &other_harness));
        let review = item("rev", AttentionKind::ReviewCandidate, 0);
        assert!(!batchable(&a, &review));

        let all = vec![b.clone(), q, approval("c", "cargo build"), medium.clone()];
        let batch = batch_with(&a, &all);
        let got: Vec<&str> = batch.iter().map(|i| i.key.object_id.as_str()).collect();
        assert_eq!(got, vec!["a", "b", "m"]);
        assert!(batch_with(&review, &all).is_empty());
    }

    #[test]
    fn batch_groups_partition_equivalent_approvals_only() {
        let items = vec![
            approval("a", "cargo test"),
            approval("c", "cargo build"),
            approval("b", "cargo test"),
            item("q", AttentionKind::ReviewCandidate, 0),
            approval("d", "cargo build"),
            approval("e", "npm test"),
        ];
        let groups = batch_groups(&items);
        assert_eq!(groups, vec![vec![0, 2], vec![1, 4]]);
        let members: Vec<&AttentionItem> = groups[0].iter().map(|&i| &items[i]).collect();
        let id = batch_id(&members);
        let rev: Vec<&AttentionItem> = members.iter().rev().copied().collect();
        assert_eq!(id, batch_id(&rev), "order-independent");
        assert_ne!(id, batch_id(&[&items[1], &items[4]]));
        assert!(batch_groups(&items[3..4]).is_empty());
    }

    #[test]
    fn expired_interactions_are_not_answerable() {
        let mut a = approval("a", "cargo test");
        assert!(!expired(&a, NOW));
        a.deadline_ms = Some(NOW + 1);
        assert!(!expired(&a, NOW));
        a.deadline_ms = Some(NOW);
        assert!(expired(&a, NOW));
        // Ranking drops it too (reconciled at the source).
        assert!(rank(&[a], NOW, &p()).is_empty());
        let mut r = item("r", AttentionKind::ReviewCandidate, 0);
        r.deadline_ms = Some(NOW - 1);
        assert!(!expired(&r, NOW));
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(45_000), "45s");
        assert_eq!(format_duration(12 * MIN), "12m");
        assert_eq!(format_duration(65 * MIN), "1h 5m");
        assert_eq!(format_duration(120 * MIN), "2h");
        assert_eq!(format_duration(3 * 86_400_000), "3d");
        assert_eq!(format_duration(-5), "0s");
    }

    #[test]
    fn serde_shapes() {
        let r = rank(&[item("a", AttentionKind::ReviewCandidate, 0)], NOW, &p());
        let j = serde_json::to_value(&r[0]).unwrap();
        assert_eq!(j["class"], "review_candidate");
        assert_eq!(j["item"]["kind"], "review_candidate");
        assert_eq!(j["item"]["effort"], "unknown");
        let back: RankedItem = serde_json::from_value(j).unwrap();
        assert_eq!(back, r[0]);
        let w = serde_json::to_value(WakeReason::DeliveryUncertainty).unwrap();
        assert_eq!(w["reason"], "delivery_uncertainty");
    }
}
