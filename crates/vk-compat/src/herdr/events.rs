//! Event projection: Vibeke events → Herdr event names (07 §8.3 "Initial event translation").
//!
//! Herdr subscription types use the dotted form (`pane.agent_status_changed`); streamed events
//! carry the snake-case form in their `event` field (`pane_agent_status_changed`). Plugin
//! `[[events]] on = …` hooks use the dotted form. The [`Projector`] is stateful because several
//! Herdr events have no single Vibeke counterpart: focus changes are derived per tab/workspace,
//! and `pane.agent_status_changed` fires only when the *mapped* Herdr status changes (no event
//! for `needs_approval → needs_answer`, both `blocked`).

use std::collections::HashMap;

/// Event names of the baseline as far as the spec and the plugin corpus name them. The full
/// list comes from the baseline schema (07 §8.0) and is still to be captured.
pub const BASELINE_EVENTS: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.renamed",
    "workspace.closed",
    "workspace.focused",
    "workspace.moved",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "tab.moved",
    "pane.created",
    "pane.closed",
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

/// Subscription types that require a `pane_id` (Herdr rule, 07 §8.3).
pub const PANE_SCOPED: &[&str] = &[
    "pane.agent_status_changed",
    "pane.scroll_changed",
    "pane.output_matched",
];

/// Herdr events this slice can emit from Vibeke's event log; the rest of [`BASELINE_EVENTS`]
/// are accepted in subscriptions and manifests but never fire yet (inventory: missing).
pub const PROJECTED: &[&str] = &[
    "workspace.created",
    "workspace.updated",
    "workspace.renamed",
    "workspace.closed",
    "workspace.focused",
    "workspace.moved",
    "tab.created",
    "tab.closed",
    "tab.focused",
    "tab.renamed",
    "pane.created",
    "pane.closed",
    "pane.focused",
    "pane.exited",
    "pane.agent_detected",
    "pane.agent_status_changed",
    "layout.updated",
    "worktree.removed",
];

/// `pane.agent_status_changed` → `pane_agent_status_changed`.
pub fn wire_name(dotted: &str) -> String {
    dotted.replace('.', "_")
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
}

/// Stateful Vibeke → Herdr event translation; one per subscription or hook dispatcher.
#[derive(Debug, Default, Clone)]
pub struct Projector {
    focused_tab: Option<String>,
    focused_ws: Option<String>,
    status: HashMap<String, String>,
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
            "workspace.moved" => out.push("workspace.moved"),
            "tab.created" => out.push("tab.created"),
            "tab.closed" => {
                if self.focused_tab.as_deref() == e.tab {
                    self.focused_tab = None;
                }
                out.push("tab.closed")
            }
            "tab.renamed" => out.push("tab.renamed"),
            "tab.layout_changed" | "layout.applied" => out.push("layout.updated"),
            "pane.created" => out.push("pane.created"),
            "pane.closed" => {
                if let Some(p) = e.pane {
                    self.status.remove(p);
                }
                out.push("pane.closed")
            }
            "pane.exited" => out.push("pane.exited"),
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
                out.push("pane.agent_detected");
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
            "worktree.removed" => out.push("worktree.removed"),
            _ => {}
        }
        out
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
        out.push(Subscription {
            kind: kind.to_string(),
            pane_id,
        });
    }
    Ok(out)
}

/// Whether `sub` wants a Herdr event about `pane` (Herdr pane id).
pub fn matches(sub: &Subscription, event: &str, pane: Option<&str>) -> bool {
    sub.kind == event && sub.pane_id.as_deref().is_none_or(|want| Some(want) == pane)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input<'a>(kind: &'a str, ws: &'a str, tab: &'a str, pane: &'a str) -> Input<'a> {
        Input {
            kind,
            workspace: Some(ws),
            tab: Some(tab),
            pane: Some(pane),
            status_after: None,
        }
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
