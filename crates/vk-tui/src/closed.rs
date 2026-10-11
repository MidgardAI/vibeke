//! How a closed interaction ended, in words. The model says only `Cancelled` for three
//! different things: the user dismissed it, Vibeke closed it on its own because the agent
//! moved on without the dialog (`closed_by: "heuristic"`, nobody answered), or a client
//! cancelled it. The pushed `interaction.cancelled` event says which; this keeps it per
//! interaction so the inbox, the card and the batch view never claim an answer that did not
//! happen.

use std::collections::HashMap;

use serde_json::Value;
use vk_proto::model::{Interaction, InteractionStatus};

use crate::app::App;

/// Why a cancelled interaction closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closed {
    /// Vibeke closed it because the agent moved on without the dialog (nobody answered).
    Heuristic,
    /// The user dismissed it.
    Dismissed,
    /// Cancelled for another reason (the event's `reason`).
    Other(String),
}

/// Most reasons kept (the oldest go first).
const MAX: usize = 500;

#[derive(Debug, Default)]
pub struct State {
    reasons: HashMap<(usize, String), Closed>,
    order: Vec<(usize, String)>,
}

impl State {
    pub fn get(&self, mi: usize, id: &str) -> Option<&Closed> {
        self.reasons.get(&(mi, id.to_string()))
    }

    pub fn insert(&mut self, mi: usize, id: &str, c: Closed) {
        let k = (mi, id.to_string());
        if self.reasons.insert(k.clone(), c).is_none() {
            self.order.push(k);
            if self.order.len() > MAX {
                let old = self.order.remove(0);
                self.reasons.remove(&old);
            }
        }
    }
}

/// The reason of an `interaction.cancelled` event's data.
pub fn of_event(data: &Value) -> Closed {
    if data.get("closed_by").and_then(Value::as_str) == Some("heuristic") {
        return Closed::Heuristic;
    }
    match data.get("reason").and_then(Value::as_str) {
        Some("dismissed") => Closed::Dismissed,
        Some(r) => Closed::Other(r.to_string()),
        None => Closed::Other(String::new()),
    }
}

/// Pushed `interaction.*` events: remember why a cancelled one closed.
pub fn on_event(app: &mut App, mi: usize, kind: &str, v: &Value) {
    if kind != "interaction.cancelled" {
        return;
    }
    let Some(id) = v
        .pointer("/subject/interaction")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    else {
        return;
    };
    let c = of_event(&v["data"]);
    app.ux.closed.insert(mi, id, c);
}

/// How a closed interaction ended, for a status line (`None` while it is open).
pub fn text(app: &App, mi: usize, it: &Interaction) -> Option<String> {
    text_with(app.ux.closed.get(mi, &it.id), it)
}

pub fn text_with(reason: Option<&Closed>, it: &Interaction) -> Option<String> {
    Some(match it.status {
        InteractionStatus::Open => return None,
        InteractionStatus::Answered => match it.answered_by.as_deref() {
            Some(by) => match crate::gateway::answered_text(by) {
                Some(t) => t,
                None => format!("answered by {by}"),
            },
            None => "answered".into(),
        },
        InteractionStatus::ResolvedElsewhere => "answered in the pane".into(),
        InteractionStatus::Expired => "expired without an answer".into(),
        InteractionStatus::Cancelled => match reason {
            Some(Closed::Heuristic) => {
                "closed automatically: the agent moved on (nobody answered)".into()
            }
            Some(Closed::Dismissed) => "dismissed".into(),
            _ => "closed without an answer".into(),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn heuristic_closes_and_dismissals_never_read_as_answers() {
        assert_eq!(
            of_event(
                &json!({"reason": "no_dialog_seen", "closed_by": "heuristic", "answered": false})
            ),
            Closed::Heuristic
        );
        assert_eq!(of_event(&json!({"reason": "dismissed"})), Closed::Dismissed);
        assert_eq!(
            of_event(&json!({"reason": "turn_ended"})),
            Closed::Other("turn_ended".into())
        );
        let mut s = State::default();
        for i in 0..(MAX + 10) {
            s.insert(0, &format!("i{i}"), Closed::Dismissed);
        }
        assert!(s.get(0, "i0").is_none());
        assert_eq!(s.get(0, &format!("i{}", MAX + 9)), Some(&Closed::Dismissed));
    }

    #[test]
    fn each_way_of_closing_has_its_own_words() {
        let mut it = crate::app::test_interaction("i1", "p1", "Run tests", 1);
        assert_eq!(text_with(None, &it), None);
        it.status = InteractionStatus::Cancelled;
        let t = text_with(Some(&Closed::Heuristic), &it).unwrap();
        assert!(
            t.starts_with("closed automatically") && !t.contains("answered "),
            "{t}"
        );
        assert_eq!(
            text_with(Some(&Closed::Dismissed), &it).unwrap(),
            "dismissed"
        );
        assert_eq!(text_with(None, &it).unwrap(), "closed without an answer");
        it.status = InteractionStatus::ResolvedElsewhere;
        assert_eq!(text_with(None, &it).unwrap(), "answered in the pane");
        it.status = InteractionStatus::Answered;
        it.answered_by = Some("gateway:d1(phone)".into());
        assert!(text_with(None, &it).unwrap().starts_with("answered on"));
    }
}
