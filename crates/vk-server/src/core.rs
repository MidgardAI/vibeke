//! In-memory projections of the state tables plus the store. All mutations go through
//! [`Core::commit`], which writes entities and events in one SQLite transaction and only then
//! applies the change in memory (02 §4a: a failed commit means the mutation didn't happen).

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use vk_proto::model::*;
use vk_store::{Event, Mutation, Store, now_ms};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Counters {
    pub workspace: u32,
    pub pane: u32,
    pub run: u32,
    pub interaction: u32,
    pub task: u32,
    pub notification: u32,
    pub tab: HashMap<String, u32>,
    #[serde(default)]
    pub group: u32,
}

pub struct Core {
    pub store: Store,
    pub model: SessionModel,
    pub counters: Counters,
    pub notifications: Vec<Notification>,
    /// Set by [`Core::commit`] when the store refused an `ephemeral` transaction and it was
    /// applied in memory only (02 §4a); carries the storage error. [`crate::Server::commit`]
    /// takes it and enters degraded mode.
    pub ephemeral_hit: Option<String>,
}

/// A mutation being prepared against the current model. Entities put here are persisted and
/// then applied to the model by [`Core::commit`].
#[derive(Default)]
pub struct Tx {
    pub m: Mutation,
    pub workspaces: Vec<Workspace>,
    pub tabs: Vec<Tab>,
    pub panes: Vec<Pane>,
    pub runs: Vec<AgentRun>,
    pub interactions: Vec<Interaction>,
    pub tasks: Vec<Task>,
    pub groups: Vec<Group>,
    pub removed: Vec<(&'static str, String)>,
    pub counters: bool,
    /// A UI convenience (focus, unread marks): if storage is unavailable it is applied in
    /// memory only, flagged ephemeral and lost on restart (02 §4a). Never set for anything a
    /// restart or another client must see (interactions, tasks, layout).
    pub ephemeral: bool,
}

impl Tx {
    pub fn new() -> Self {
        Tx::default()
    }
    pub fn ws(&mut self, w: Workspace) -> &mut Self {
        self.m.put("workspace", &w.id, Some(&w.handle), &w);
        self.workspaces.push(w);
        self
    }
    pub fn tab(&mut self, t: Tab) -> &mut Self {
        self.m.put("tab", &t.id, Some(&t.handle), &t);
        self.tabs.push(t);
        self
    }
    pub fn pane(&mut self, p: Pane) -> &mut Self {
        self.m.put("pane", &p.id, Some(&p.handle), &p);
        self.panes.push(p);
        self
    }
    pub fn run(&mut self, r: AgentRun) -> &mut Self {
        if r.ended_at_ms.is_some() {
            self.m.close("run", &r.id, Some(&r.handle), &r);
        } else {
            self.m.put("run", &r.id, Some(&r.handle), &r);
        }
        self.runs.push(r);
        self
    }
    pub fn interaction(&mut self, i: Interaction) -> &mut Self {
        if i.status == InteractionStatus::Open
            || matches!(
                i.delivery,
                DeliveryState::Delivering | DeliveryState::DecisionRecorded
            )
        {
            self.m.put("interaction", &i.id, Some(&i.handle), &i);
        } else {
            self.m.close("interaction", &i.id, Some(&i.handle), &i);
        }
        self.interactions.push(i);
        self
    }
    pub fn task(&mut self, t: Task) -> &mut Self {
        if matches!(t.status.as_str(), "archived") {
            self.m.close("task", &t.id, Some(&t.handle), &t);
        } else {
            self.m.put("task", &t.id, Some(&t.handle), &t);
        }
        self.tasks.push(t);
        self
    }
    pub fn group(&mut self, g: Group) -> &mut Self {
        self.m.put("group", &g.id, Some(&g.handle), &g);
        self.groups.push(g);
        self
    }
    pub fn close_group(&mut self, g: &Group) -> &mut Self {
        self.m.close("group", &g.id, Some(&g.handle), g);
        self.removed.push(("group", g.id.clone()));
        self
    }
    pub fn close_ws(&mut self, w: &Workspace) -> &mut Self {
        self.m.close("workspace", &w.id, Some(&w.handle), w);
        self.removed.push(("workspace", w.id.clone()));
        self
    }
    pub fn close_tab(&mut self, t: &Tab) -> &mut Self {
        self.m.close("tab", &t.id, Some(&t.handle), t);
        self.removed.push(("tab", t.id.clone()));
        self
    }
    pub fn close_pane(&mut self, p: &Pane) -> &mut Self {
        self.m.close("pane", &p.id, Some(&p.handle), p);
        self.m.holder_delete(&p.id);
        self.removed.push(("pane", p.id.clone()));
        self
    }
    pub fn event(&mut self, kind: &str, subject: Value, data: Value) -> &mut Self {
        self.m.event(kind, subject, data);
        self
    }
    pub fn event_by(&mut self, kind: &str, subject: Value, actor: Value, data: Value) -> &mut Self {
        self.m.event_by(kind, subject, actor, data);
        self
    }
}

fn upsert<T: Clone>(v: &mut Vec<T>, item: T, same: impl Fn(&T, &T) -> bool) {
    match v.iter_mut().find(|x| same(x, &item)) {
        Some(x) => *x = item,
        None => v.push(item),
    }
}

impl Core {
    pub fn load(store: Store, session: &str, machine: &str) -> Result<Self> {
        let mut model = SessionModel {
            session: session.to_string(),
            machine: machine.to_string(),
            server_version: vk_proto::VERSION.to_string(),
            ..Default::default()
        };
        model.workspaces = store.load("workspace")?;
        model.tabs = store.load("tab")?;
        model.panes = store.load("pane")?;
        model.runs = store.load("run")?;
        model.interactions = store.load("interaction")?;
        model.tasks = store.load("task")?;
        model.groups = store.load("group")?;
        model.groups.sort_by(|a, b| a.order.total_cmp(&b.order));
        model.workspaces.sort_by(|a, b| a.order.total_cmp(&b.order));
        model.tabs.sort_by(|a, b| a.order.total_cmp(&b.order));
        let counters = store
            .kv_get("server", "counters")?
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Ok(Core {
            store,
            model,
            counters,
            notifications: Vec::new(),
            ephemeral_hit: None,
        })
    }

    /// Persist then apply. Returns the committed events.
    pub fn commit(&mut self, mut tx: Tx) -> Result<Vec<Event>> {
        if tx.counters {
            tx.m.kv(
                "server",
                "counters",
                Some(serde_json::to_string(&self.counters)?),
            );
        }
        let events = match self.store.commit(std::mem::take(&mut tx.m)) {
            Ok(e) => e,
            // Pure UI convenience while storage is down: keep it in memory, emit no event.
            Err(e) if tx.ephemeral => {
                self.ephemeral_hit = Some(format!("storage unavailable: {e:#}"));
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        for w in tx.workspaces {
            upsert(&mut self.model.workspaces, w, |a, b| a.id == b.id);
        }
        for t in tx.tabs {
            upsert(&mut self.model.tabs, t, |a, b| a.id == b.id);
        }
        for p in tx.panes {
            upsert(&mut self.model.panes, p, |a, b| a.id == b.id);
        }
        for r in tx.runs {
            if r.ended_at_ms.is_some() {
                self.model.runs.retain(|x| x.id != r.id);
            } else {
                upsert(&mut self.model.runs, r, |a, b| a.id == b.id);
            }
        }
        for i in tx.interactions {
            if i.status == InteractionStatus::Open
                || matches!(
                    i.delivery,
                    DeliveryState::Delivering | DeliveryState::DecisionRecorded
                )
            {
                upsert(&mut self.model.interactions, i, |a, b| a.id == b.id);
            } else {
                self.model.interactions.retain(|x| x.id != i.id);
            }
        }
        for t in tx.tasks {
            if t.status == "archived" {
                self.model.tasks.retain(|x| x.id != t.id);
            } else {
                upsert(&mut self.model.tasks, t, |a, b| a.id == b.id);
            }
        }
        for g in tx.groups {
            upsert(&mut self.model.groups, g, |a, b| a.id == b.id);
        }
        for (kind, id) in tx.removed {
            match kind {
                "group" => self.model.groups.retain(|x| x.id != id),
                "workspace" => self.model.workspaces.retain(|x| x.id != id),
                "tab" => self.model.tabs.retain(|x| x.id != id),
                "pane" => self.model.panes.retain(|x| x.id != id),
                _ => {}
            }
        }
        self.model
            .workspaces
            .sort_by(|a, b| a.order.total_cmp(&b.order));
        self.model.tabs.sort_by(|a, b| a.order.total_cmp(&b.order));
        self.model
            .groups
            .sort_by(|a, b| a.order.total_cmp(&b.order));
        Ok(events)
    }

    // ---- lookups ------------------------------------------------------------------------

    pub fn ws(&self, id: &str) -> Option<&Workspace> {
        self.model
            .workspaces
            .iter()
            .find(|w| w.id == id || w.handle == id)
    }
    pub fn tab(&self, id: &str) -> Option<&Tab> {
        self.model
            .tabs
            .iter()
            .find(|t| t.id == id || t.handle == id)
    }
    pub fn pane(&self, id: &str) -> Option<&Pane> {
        self.model
            .panes
            .iter()
            .find(|p| p.id == id || p.handle == id)
    }
    pub fn run(&self, id: &str) -> Option<&AgentRun> {
        self.model
            .runs
            .iter()
            .find(|r| r.id == id || r.handle == id || r.name.as_deref() == Some(id))
    }
    pub fn run_for_pane(&self, pane: &str) -> Option<&AgentRun> {
        self.model.runs.iter().find(|r| r.pane == pane)
    }
    pub fn interaction(&self, id: &str) -> Option<&Interaction> {
        self.model
            .interactions
            .iter()
            .find(|i| i.id == id || i.handle == id)
    }
    pub fn task(&self, id: &str) -> Option<&Task> {
        self.model
            .tasks
            .iter()
            .find(|t| t.id == id || t.handle == id)
    }
    pub fn tabs_of(&self, ws: &str) -> Vec<&Tab> {
        self.model
            .tabs
            .iter()
            .filter(|t| t.workspace == ws)
            .collect()
    }
    pub fn panes_of_tab(&self, tab: &str) -> Vec<&Pane> {
        self.model.panes.iter().filter(|p| p.tab == tab).collect()
    }

    // ---- id allocation ------------------------------------------------------------------

    pub fn next_ws_handle(&mut self) -> String {
        self.counters.workspace += 1;
        format!("w{}", self.counters.workspace)
    }
    pub fn next_tab_number(&mut self, ws_id: &str) -> u32 {
        let n = self.counters.tab.entry(ws_id.to_string()).or_insert(0);
        *n += 1;
        *n
    }
    pub fn next_pane_handle(&mut self, ws_handle: &str) -> String {
        self.counters.pane += 1;
        format!("{ws_handle}:p{}", self.counters.pane)
    }
    pub fn next_run_handle(&mut self) -> String {
        self.counters.run += 1;
        format!("a{}", self.counters.run)
    }
    pub fn next_interaction_handle(&mut self) -> String {
        self.counters.interaction += 1;
        format!("i{}", self.counters.interaction)
    }
    pub fn next_group_handle(&mut self) -> String {
        self.counters.group += 1;
        format!("g{}", self.counters.group)
    }
    pub fn group(&self, id: &str) -> Option<&Group> {
        self.model
            .groups
            .iter()
            .find(|g| g.id == id || g.handle == id || g.name == id)
    }
    pub fn next_task_handle(&mut self) -> String {
        self.counters.task += 1;
        format!("k{}", self.counters.task)
    }

    pub fn notify(
        &mut self,
        kind: &str,
        pane: Option<&str>,
        title: &str,
        body: &str,
        urgency: &str,
    ) -> Notification {
        self.counters.notification += 1;
        let n = Notification {
            id: format!("n{}", self.counters.notification),
            kind: kind.into(),
            pane: pane.map(Into::into),
            title: title.into(),
            body: body.into(),
            urgency: urgency.into(),
            created_at_ms: now_ms(),
            read: false,
            channels: vec![],
        };
        self.notifications.push(n.clone());
        if self.notifications.len() > 500 {
            self.notifications.remove(0);
        }
        n
    }
}

pub fn subject_pane(p: &Pane) -> Value {
    json!({"pane": p.id, "pane_handle": p.handle, "tab": p.tab, "workspace": p.workspace})
}

pub fn ulid() -> String {
    ulid::Ulid::new().to_string()
}
