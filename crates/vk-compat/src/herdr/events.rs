//! Event projection: Vibeke events → Herdr event names (07 §8.3 "Initial event translation").
//!
//! Herdr subscription types use the dotted form (`pane.agent_status_changed`); streamed events
//! carry the snake-case form in their `event` field (`pane_agent_status_changed`). Plugin
//! `[[events]] on = …` hooks use the dotted form. The [`Projector`] is stateful because several
//! Herdr events have no single Vibeke counterpart: focus changes are derived per tab/workspace,
//! and `pane.agent_status_changed` fires only when the *mapped* Herdr status changes (no event
//! for `needs_approval → needs_answer`, both `blocked`).

use std::collections::HashMap;

/// Subscription types of the baseline (Herdr 0.9.3 `Subscription`, dotted form), plus
/// `pane.output_changed`: Herdr has that `EventKind` but no subscription for it, and Vibeke
/// accepts it (it never fires, see [`PROJECTED`]).
pub const BASELINE_EVENTS: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.metadata_updated",
    "workspace.renamed",
    "workspace.closed",
    "workspace.focused",
    "workspace.moved",
    "workspace.reordered",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.moved",
    "pane.output_changed",
    "pane.exited",
    "pane.agent_detected",
    "pane.agent_status_changed",
    "pane.output_matched",
    "pane.scroll_changed",
    "layout.updated",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
];

/// Events that run `[[events]]` hooks (Herdr `PLUGIN_HOOK_EVENT_KINDS`). Herdr fires no hook
/// for `workspace.metadata_updated`, `pane.updated`, `pane.output_changed` or `layout.updated`.
pub const HOOK_EVENTS: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.closed",
    "workspace.renamed",
    "workspace.moved",
    "workspace.reordered",
    "workspace.focused",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
    "tab.created",
    "tab.closed",
    "tab.renamed",
    "tab.moved",
    "tab.focused",
    "pane.created",
    "pane.closed",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
    "pane.agent_status_changed",
];

/// Pane-scoped subscriptions whose stream events keep the dotted name in `event` and carry no
/// `type` in `data` (Herdr `SubscriptionEventEnvelope`). Every other streamed event is a full
/// `EventEnvelope` ([`envelope`]).
pub const SUBSCRIPTION_ONLY: &[&str] = &[
    "pane.output_matched",
    "pane.agent_status_changed",
    "pane.scroll_changed",
];

/// Subscription types that require a `pane_id` (Herdr rule, 07 §8.3).
pub const PANE_SCOPED: &[&str] = &[
    "pane.agent_status_changed",
    "pane.scroll_changed",
    "pane.output_matched",
];

/// Herdr events Vibeke emits from its event log; the rest of [`BASELINE_EVENTS`] are accepted
/// in subscriptions and manifests but never fire (inventory: missing).
pub const PROJECTED: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.metadata_updated",
    "workspace.renamed",
    "workspace.closed",
    "workspace.focused",
    "workspace.moved",
    "workspace.reordered",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
    "pane.updated",
    "pane.focused",
    "pane.moved",
    "pane.exited",
    "pane.agent_detected",
    "pane.agent_status_changed",
    "pane.output_matched",
    "pane.scroll_changed",
    "layout.updated",
    "worktree.created",
    "worktree.opened",
    "worktree.removed",
];

/// `pane.agent_status_changed` → `pane_agent_status_changed`.
pub fn wire_name(dotted: &str) -> String {
    dotted.replace('.', "_")
}

/// Herdr's `EventEnvelope` for one event: `{"event": wire, "data": {"type": wire, …data}}`.
/// This is `HERDR_PLUGIN_EVENT_JSON` for hooks and the streamed form of most subscriptions.
pub fn envelope(dotted: &str, data: serde_json::Value) -> serde_json::Value {
    let wire = wire_name(dotted);
    let mut d = serde_json::Map::new();
    d.insert("type".into(), serde_json::Value::String(wire.clone()));
    if let serde_json::Value::Object(f) = data {
        d.extend(f);
    }
    serde_json::json!({"event": wire, "data": d})
}

/// One line of an `events.subscribe` stream: [`envelope`], except the pane-scoped
/// [`SUBSCRIPTION_ONLY`] events, which keep their dotted name and plain data.
pub fn stream_event(dotted: &str, data: serde_json::Value) -> serde_json::Value {
    if SUBSCRIPTION_ONLY.contains(&dotted) {
        serde_json::json!({"event": dotted, "data": data})
    } else {
        envelope(dotted, data)
    }
}

/// What the projector needs to know about one Vibeke event.
#[derive(Debug, Clone, Default)]
pub struct Input<'a> {
    /// Vibeke event type (`pane.focused`, `agent.state_changed`, …).
    pub kind: &'a str,
    pub workspace: Option<&'a str>,
    pub tab: Option<&'a str>,
    pub pane: Option<&'a str>,
    /// The pane's Herdr agent status after this event (agent/interaction events), computed by
    /// the server from its model ([`super::status::agent_status`]).
    pub status_after: Option<&'a str>,
    /// Event time in ms (0: unknown, no debouncing).
    pub at_ms: u64,
}

/// `pane.agent_detected` fires at most once per pane in this window (07 §8.3).
pub const DETECT_DEBOUNCE_MS: u64 = 250;

/// Stateful Vibeke → Herdr event translation; one per subscription or hook dispatcher.
#[derive(Debug, Default, Clone)]
pub struct Projector {
    focused_tab: Option<String>,
    focused_ws: Option<String>,
    status: HashMap<String, String>,
    detected_at: HashMap<String, u64>,
}

impl Projector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the current focus and pane statuses so the first event after subscribing does not
    /// report a spurious change.
    pub fn seed(
        &mut self,
        tab: Option<&str>,
        ws: Option<&str>,
        statuses: impl IntoIterator<Item = (String, String)>,
    ) {
        self.focused_tab = tab.map(str::to_string);
        self.focused_ws = ws.map(str::to_string);
        self.status.extend(statuses);
    }

    /// Herdr event names (dotted) produced by one Vibeke event, in emission order.
    pub fn project(&mut self, e: &Input) -> Vec<&'static str> {
        let mut out = Vec::new();
        match e.kind {
            "workspace.created" => out.push("workspace.created"),
            "workspace.renamed" => out.extend(["workspace.renamed", "workspace.updated"]),
            "workspace.closed" => {
                if self.focused_ws.as_deref() == e.workspace {
                    self.focused_ws = None;
                }
                out.push("workspace.closed")
            }
            // `workspace.reordered` is declared by a corpus plugin; a move is the only
            // reordering Vibeke has.
            "workspace.moved" => out.extend(["workspace.moved", "workspace.reordered"]),
            "tab.created" => out.push("tab.created"),
            "tab.closed" => {
                if self.focused_tab.as_deref() == e.tab {
                    self.focused_tab = None;
                }
                out.push("tab.closed")
            }
            "tab.renamed" => out.push("tab.renamed"),
            "tab.moved" => out.push("tab.moved"),
            "tab.layout_changed" | "layout.applied" => out.push("layout.updated"),
            "pane.created" => out.push("pane.created"),
            "pane.closed" => {
                if let Some(p) = e.pane {
                    self.status.remove(p);
                    self.detected_at.remove(p);
                }
                out.push("pane.closed")
            }
            "pane.exited" => out.push("pane.exited"),
            // Herdr reports a pane's changed title, cwd or reported metadata as `pane.updated`
            // and a workspace's reported tokens as `workspace.metadata_updated`.
            "pane.title_changed" | "pane.cwd_changed" | "pane.metadata_reported" => {
                out.push("pane.updated")
            }
            "workspace.metadata_reported" => out.push("workspace.metadata_updated"),
            "pane.moved" => out.push("pane.moved"),
            // Emitted by the compat `pane.wait_for_output` matcher (07 §8.3).
            "pane.output_matched" => out.push("pane.output_matched"),
            // A client's scrollback viewport moved (TUI `ClientFrame::ScrollView`).
            "pane.scroll_changed" => out.push("pane.scroll_changed"),
            "pane.focused" => {
                if e.workspace.is_some() && self.focused_ws.as_deref() != e.workspace {
                    self.focused_ws = e.workspace.map(str::to_string);
                    out.push("workspace.focused");
                }
                if e.tab.is_some() && self.focused_tab.as_deref() != e.tab {
                    self.focused_tab = e.tab.map(str::to_string);
                    out.push("tab.focused");
                }
                out.push("pane.focused");
            }
            "agent.detected" | "agent.started" => {
                if self.detect_admitted(e) {
                    out.push("pane.agent_detected");
                }
                self.status_change(e, &mut out);
            }
            "agent.state_changed"
            | "agent.exited"
            | "agent.session_ended"
            | "agent.turn_completed"
            | "interaction.opened"
            | "interaction.updated"
            | "interaction.resolved"
            | "interaction.cancelled"
            | "interaction.expired"
            | "pane.marked_unread" => self.status_change(e, &mut out),
            "worktree.created" => out.push("worktree.created"),
            "worktree.opened" => out.push("worktree.opened"),
            "worktree.removed" => out.push("worktree.removed"),
            _ => {}
        }
        out
    }

    /// The 250 ms debounce of `pane.agent_detected`: a second detection of the same pane inside
    /// the window is folded into the first.
    fn detect_admitted(&mut self, e: &Input) -> bool {
        let (Some(pane), at) = (e.pane, e.at_ms) else {
            return true;
        };
        if at == 0 {
            return true;
        }
        match self.detected_at.get(pane) {
            Some(prev) if at.saturating_sub(*prev) < DETECT_DEBOUNCE_MS => false,
            _ => {
                self.detected_at.insert(pane.to_string(), at);
                true
            }
        }
    }

    fn status_change(&mut self, e: &Input, out: &mut Vec<&'static str>) {
        let (Some(pane), Some(now)) = (e.pane, e.status_after) else {
            return;
        };
        if self.status.get(pane).map(String::as_str) != Some(now) {
            self.status.insert(pane.to_string(), now.to_string());
            out.push("pane.agent_status_changed");
        }
    }
}

/// One Herdr subscription (`{type, pane_id?}`).
#[derive(Debug, Clone, PartialEq)]
pub struct Subscription {
    pub kind: String,
    pub pane_id: Option<String>,
    /// `pane.output_matched` with Herdr's `source`, `match` and `lines`: an output watcher.
    pub output: Option<OutputWatch>,
}

/// Herdr's `pane.output_matched` subscription matcher: the pane is read from `source`
/// (`recent` reads `recent_unwrapped`, as Herdr does) and an event fires each time a line
/// starts to match (not again while it keeps matching).
#[derive(Debug, Clone, PartialEq)]
pub struct OutputWatch {
    /// The source the read uses (after Herdr's `recent` → `recent_unwrapped`).
    pub source: String,
    pub lines: Option<u64>,
    /// `true` for `{type: "regex"}`, `false` for `{type: "substring"}`.
    pub regex: bool,
    pub value: String,
}

/// Herdr's `{type: substring|regex, value}` (`OutputMatch`).
pub fn parse_output_match(m: &serde_json::Value) -> Result<(bool, String), (&'static str, String)> {
    let kind = m.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let value = m
        .get("value")
        .and_then(|v| v.as_str())
        .ok_or(("invalid_params", "match.value is required".to_string()))?;
    match kind {
        "substring" => Ok((false, value.to_string())),
        "regex" => Ok((true, value.to_string())),
        other => Err(("invalid_params", format!("unknown match type `{other}`"))),
    }
}

/// Validate `events.subscribe {subscriptions: [...]}` params. Errors are `(code, message)`.
pub fn parse_subscriptions(
    params: &serde_json::Value,
) -> Result<Vec<Subscription>, (&'static str, String)> {
    let list = params
        .get("subscriptions")
        .and_then(|v| v.as_array())
        .ok_or((
            "invalid_params",
            "subscriptions must be an array".to_string(),
        ))?;
    let mut out = Vec::new();
    for s in list {
        let kind = s.get("type").and_then(|v| v.as_str()).ok_or((
            "invalid_params",
            "subscription type is required".to_string(),
        ))?;
        if !BASELINE_EVENTS.contains(&kind) {
            return Err(("invalid_params", format!("unknown event type `{kind}`")));
        }
        let pane_id = s
            .get("pane_id")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        if PANE_SCOPED.contains(&kind) && pane_id.is_none() {
            return Err(("invalid_params", format!("`{kind}` requires pane_id")));
        }
        // Herdr requires `source` and `match` here; Vibeke's older form without them only
        // relays matches that `pane.wait_for_output` calls report.
        let output = match (kind, s.get("match")) {
            ("pane.output_matched", Some(m)) => {
                let (regex, value) = parse_output_match(m)?;
                let source = match s.get("source").and_then(|v| v.as_str()) {
                    Some("recent") => "recent_unwrapped",
                    Some(x @ ("visible" | "recent_unwrapped" | "detection")) => x,
                    Some(x) => return Err(("invalid_params", format!("unknown source `{x}`"))),
                    None => return Err(("invalid_params", "source is required".to_string())),
                };
                Some(OutputWatch {
                    source: source.to_string(),
                    lines: s.get("lines").and_then(|v| v.as_u64()),
                    regex,
                    value,
                })
            }
            _ => None,
        };
        out.push(Subscription {
            kind: kind.to_string(),
            pane_id,
            output,
        });
    }
    Ok(out)
}

/// The `event` names of Herdr's `EventMatch` (`events.wait {match_event}`).
pub const EVENT_MATCH_EVENTS: &[&str] = &[
    "workspace_created",
    "workspace_updated",
    "workspace_closed",
    "workspace_renamed",
    "workspace_moved",
    "workspace_focused",
    "tab_created",
    "tab_closed",
    "tab_renamed",
    "tab_moved",
    "tab_focused",
    "pane_created",
    "pane_closed",
    "pane_focused",
    "pane_moved",
    "pane_output_changed",
    "pane_exited",
    "pane_agent_detected",
    "pane_agent_status_changed",
];

/// Herdr's `AgentStatus` values.
pub const AGENT_STATUSES: &[&str] = &["idle", "working", "blocked", "done", "unknown"];

/// What Herdr 0.9.3's `events.wait {match_event, timeout_ms?}` can wait for: a pane reaching
/// an agent status. Every other `EventMatch` is answered `unsupported_event_wait_match`.
#[derive(Debug, Clone, PartialEq)]
pub struct StatusWait {
    pub pane_id: String,
    pub agent_status: String,
    pub timeout_ms: Option<u64>,
}

/// Herdr's message for a match `events.wait` does not wait for.
pub const UNSUPPORTED_WAIT_MATCH: &str = "events.wait currently supports pane agent status matches";

/// Validate Herdr's `events.wait` params. Errors are `(code, message)`.
pub fn parse_event_match(params: &serde_json::Value) -> Result<StatusWait, (&'static str, String)> {
    let m = params
        .get("match_event")
        .filter(|m| m.is_object())
        .ok_or(("invalid_params", "match_event is required".to_string()))?;
    let event = m.get("event").and_then(|v| v.as_str()).ok_or((
        "invalid_params",
        "match_event.event is required".to_string(),
    ))?;
    if !EVENT_MATCH_EVENTS.contains(&event) {
        return Err((
            "invalid_params",
            format!("unknown match_event.event `{event}`"),
        ));
    }
    if event != "pane_agent_status_changed" {
        return Err((
            "unsupported_event_wait_match",
            UNSUPPORTED_WAIT_MATCH.into(),
        ));
    }
    let pane_id = m.get("pane_id").and_then(|v| v.as_str()).ok_or((
        "invalid_params",
        "match_event.pane_id is required".to_string(),
    ))?;
    let agent_status = m.get("agent_status").and_then(|v| v.as_str()).ok_or((
        "invalid_params",
        "match_event.agent_status is required".to_string(),
    ))?;
    if !AGENT_STATUSES.contains(&agent_status) {
        return Err((
            "invalid_params",
            format!("unknown agent_status `{agent_status}`"),
        ));
    }
    let timeout_ms = match params.get("timeout_ms") {
        None | Some(serde_json::Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or((
            "invalid_params",
            "timeout_ms must be a non-negative integer".to_string(),
        ))?),
    };
    Ok(StatusWait {
        pane_id: pane_id.to_string(),
        agent_status: agent_status.to_string(),
        timeout_ms,
    })
}

/// Whether `sub` wants a Herdr event about `pane` (Herdr pane id).
pub fn matches(sub: &Subscription, event: &str, pane: Option<&str>) -> bool {
    // An output watcher produces its own events; it does not relay other matches.
    sub.output.is_none()
        && sub.kind == event
        && sub.pane_id.as_deref().is_none_or(|want| Some(want) == pane)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn output_subscriptions_carry_herdrs_matcher() {
        let subs = parse_subscriptions(&json!({"subscriptions": [
            {"type": "pane.output_matched", "pane_id": "w1:p1", "source": "recent", "match": {"type": "regex", "value": "ok$"}, "lines": 20},
            {"type": "pane.output_matched", "pane_id": "w1:p1"},
        ]}))
        .unwrap();
        assert_eq!(
            subs[0].output,
            Some(OutputWatch {
                source: "recent_unwrapped".into(),
                lines: Some(20),
                regex: true,
                value: "ok$".into(),
            })
        );
        assert!(!matches(&subs[0], "pane.output_matched", Some("w1:p1")));
        assert_eq!(subs[1].output, None);
        assert!(matches(&subs[1], "pane.output_matched", Some("w1:p1")));
        for bad in [
            json!({"type": "pane.output_matched", "pane_id": "w1:p1", "match": {"type": "substring", "value": "x"}}),
            json!({"type": "pane.output_matched", "pane_id": "w1:p1", "source": "recent", "match": {"type": "glob", "value": "x"}}),
            json!({"type": "pane.output_matched", "pane_id": "w1:p1", "source": "recent", "match": {"type": "substring"}}),
        ] {
            let e = parse_subscriptions(&json!({"subscriptions": [bad]})).unwrap_err();
            assert_eq!(e.0, "invalid_params", "{e:?}");
        }
    }

    #[test]
    fn event_match_waits_only_for_agent_status() {
        let ok = parse_event_match(&json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": "w1:p1", "agent_status": "idle"}, "timeout_ms": 500})).unwrap();
        assert_eq!(
            ok,
            StatusWait {
                pane_id: "w1:p1".into(),
                agent_status: "idle".into(),
                timeout_ms: Some(500),
            }
        );
        let no_timeout = parse_event_match(&json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": "w1:p1", "agent_status": "done"}})).unwrap();
        assert_eq!(no_timeout.timeout_ms, None);
        let e = parse_event_match(&json!({"match_event": {"event": "tab_created"}})).unwrap_err();
        assert_eq!(
            e,
            (
                "unsupported_event_wait_match",
                UNSUPPORTED_WAIT_MATCH.to_string()
            )
        );
        for bad in [
            json!({}),
            json!({"match_event": "x"}),
            json!({"match_event": {"event": "nope"}}),
            json!({"match_event": {"event": "pane_agent_status_changed", "agent_status": "idle"}}),
            json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": "w1:p1"}}),
            json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": "w1:p1", "agent_status": "sleepy"}}),
            json!({"match_event": {"event": "pane_agent_status_changed", "pane_id": "w1:p1", "agent_status": "idle"}, "timeout_ms": -1}),
        ] {
            assert_eq!(
                parse_event_match(&bad).unwrap_err().0,
                "invalid_params",
                "{bad}"
            );
        }
    }

    fn input<'a>(kind: &'a str, ws: &'a str, tab: &'a str, pane: &'a str) -> Input<'a> {
        Input {
            kind,
            workspace: Some(ws),
            tab: Some(tab),
            pane: Some(pane),
            status_after: None,
            at_ms: 0,
        }
    }

    #[test]
    fn agent_detected_is_debounced_per_pane() {
        let mut p = Projector::new();
        let det = |p: &mut Projector, pane: &'static str, at_ms: u64, s: &'static str| {
            p.project(&Input {
                kind: "agent.detected",
                pane: Some(pane),
                status_after: Some(s),
                at_ms,
                ..Default::default()
            })
        };
        assert_eq!(
            det(&mut p, "w1:p1", 1000, "idle"),
            vec!["pane.agent_detected", "pane.agent_status_changed"]
        );
        // Inside 250 ms: folded; a status change still reports.
        assert_eq!(
            det(&mut p, "w1:p1", 1200, "working"),
            vec!["pane.agent_status_changed"]
        );
        // Another pane is independent.
        assert_eq!(
            det(&mut p, "w1:p2", 1200, "idle"),
            vec!["pane.agent_detected", "pane.agent_status_changed"]
        );
        // After the window it fires again.
        assert_eq!(
            det(&mut p, "w1:p1", 1300, "working"),
            vec!["pane.agent_detected"]
        );
        // Unknown time never debounces.
        assert_eq!(
            det(&mut p, "w1:p1", 0, "working"),
            vec!["pane.agent_detected"]
        );
    }

    #[test]
    fn a_move_also_reports_a_reorder() {
        let mut p = Projector::new();
        let out = p.project(&Input {
            kind: "workspace.moved",
            ..Default::default()
        });
        assert_eq!(out, vec!["workspace.moved", "workspace.reordered"]);
    }

    #[test]
    fn focus_derives_tab_and_workspace_events() {
        let mut p = Projector::new();
        assert_eq!(
            p.project(&input("pane.focused", "w1", "w1:t1", "w1:p1")),
            vec!["workspace.focused", "tab.focused", "pane.focused"]
        );
        assert_eq!(
            p.project(&input("pane.focused", "w1", "w1:t1", "w1:p2")),
            vec!["pane.focused"]
        );
        assert_eq!(
            p.project(&input("pane.focused", "w1", "w1:t2", "w1:p3")),
            vec!["tab.focused", "pane.focused"]
        );
        p.project(&input("tab.closed", "w1", "w1:t2", ""));
        assert_eq!(
            p.project(&input("pane.focused", "w1", "w1:t2", "w1:p3")),
            vec!["tab.focused", "pane.focused"],
            "closing the focused tab forgets it"
        );
    }

    #[test]
    fn status_events_fire_only_on_mapped_change() {
        let mut p = Projector::new();
        let st = |p: &mut Projector, kind: &str, s: &str| {
            p.project(&Input {
                kind,
                pane: Some("w1:p1"),
                status_after: Some(s),
                ..Default::default()
            })
        };
        assert_eq!(
            st(&mut p, "agent.started", "idle"),
            vec!["pane.agent_detected", "pane.agent_status_changed"]
        );
        assert_eq!(
            st(&mut p, "agent.state_changed", "working"),
            vec!["pane.agent_status_changed"]
        );
        assert!(st(&mut p, "agent.state_changed", "working").is_empty());
        assert_eq!(
            st(&mut p, "interaction.opened", "blocked"),
            vec!["pane.agent_status_changed"]
        );
        // needs_approval → needs_answer: both blocked, no event.
        assert!(st(&mut p, "interaction.updated", "blocked").is_empty());
        // Seeded status suppresses the first duplicate.
        let mut q = Projector::new();
        q.seed(None, None, [("w1:p1".to_string(), "working".to_string())]);
        assert!(st(&mut q, "agent.state_changed", "working").is_empty());
    }

    #[test]
    fn simple_renames_and_layout() {
        let mut p = Projector::new();
        let k = |p: &mut Projector, kind: &str| {
            p.project(&Input {
                kind,
                ..Default::default()
            })
        };
        assert_eq!(
            k(&mut p, "workspace.renamed"),
            vec!["workspace.renamed", "workspace.updated"]
        );
        assert_eq!(k(&mut p, "tab.layout_changed"), vec!["layout.updated"]);
        assert_eq!(k(&mut p, "worktree.removed"), vec!["worktree.removed"]);
        assert_eq!(k(&mut p, "worktree.created"), vec!["worktree.created"]);
        assert_eq!(k(&mut p, "worktree.opened"), vec!["worktree.opened"]);
        assert_eq!(k(&mut p, "tab.moved"), vec!["tab.moved"]);
        assert_eq!(k(&mut p, "pane.moved"), vec!["pane.moved"]);
        assert_eq!(
            k(&mut p, "pane.output_matched"),
            vec!["pane.output_matched"]
        );
        assert!(k(&mut p, "task.created").is_empty());
        for e in PROJECTED {
            assert!(BASELINE_EVENTS.contains(e), "{e}");
        }
        assert_eq!(
            wire_name("pane.agent_status_changed"),
            "pane_agent_status_changed"
        );
    }

    #[test]
    fn envelopes_follow_herdr() {
        let e = envelope("worktree.created", json!({"worktree": {"path": "/x"}}));
        assert_eq!(
            e,
            json!({"event": "worktree_created", "data": {"type": "worktree_created", "worktree": {"path": "/x"}}})
        );
        // Pane-scoped subscription events keep the dotted name and carry no `type`.
        let s = stream_event(
            "pane.agent_status_changed",
            json!({"pane_id": "w1:p1", "workspace_id": "w1", "agent_status": "idle"}),
        );
        assert_eq!(s["event"], "pane.agent_status_changed");
        assert!(s["data"].get("type").is_none());
        let s = stream_event("tab.created", json!({"tab": {}}));
        assert_eq!(s["event"], "tab_created");
        assert_eq!(s["data"]["type"], "tab_created");
        for h in HOOK_EVENTS {
            assert!(BASELINE_EVENTS.contains(h), "{h}");
            assert!(PROJECTED.contains(h), "{h}");
        }
    }

    #[test]
    fn updates_and_metadata_project() {
        let mut p = Projector::new();
        let k = |p: &mut Projector, kind: &str| {
            p.project(&Input {
                kind,
                ..Default::default()
            })
        };
        assert_eq!(k(&mut p, "pane.title_changed"), vec!["pane.updated"]);
        assert_eq!(k(&mut p, "pane.cwd_changed"), vec!["pane.updated"]);
        assert_eq!(k(&mut p, "pane.metadata_reported"), vec!["pane.updated"]);
        assert_eq!(
            k(&mut p, "workspace.metadata_reported"),
            vec!["workspace.metadata_updated"]
        );
        let ok = parse_subscriptions(&json!({"subscriptions": [
            {"type": "workspace.metadata_updated"},
            {"type": "pane.updated"},
            {"type": "pane.output_changed"}
        ]}))
        .unwrap();
        assert_eq!(ok.len(), 3);
    }

    #[test]
    fn subscriptions_enforce_pane_rule() {
        let ok = parse_subscriptions(&json!({"subscriptions": [
            {"type": "pane.created"},
            {"type": "pane.agent_status_changed", "pane_id": "w1:p1"}
        ]}))
        .unwrap();
        assert_eq!(ok.len(), 2);
        assert!(matches(&ok[1], "pane.agent_status_changed", Some("w1:p1")));
        assert!(!matches(&ok[1], "pane.agent_status_changed", Some("w1:p2")));
        assert!(matches(&ok[0], "pane.created", Some("anything")));
        let e = parse_subscriptions(&json!({"subscriptions": [{"type": "pane.output_matched"}]}))
            .unwrap_err();
        assert_eq!(e.0, "invalid_params");
        assert!(parse_subscriptions(&json!({"subscriptions": [{"type": "nope"}]})).is_err());
        assert!(parse_subscriptions(&json!({})).is_err());
    }
}
