//! In-memory projections of the state tables plus the store. All mutations go through
//! [`crate::Server::commit`] (or [`crate::Server::commit_storage`]), which write entities and
//! events in one SQLite transaction and only then apply the change in memory (02 §4a: a failed
//! commit means the mutation didn't happen). The store write itself (`Core::apply`) is private
//! to this module, so no caller can skip the server's event broadcast, model revision and
//! degraded-mode handling.

use crate::pane::{PaneCmd, PaneRt};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use vk_proto::model::*;
use vk_store::{Event, Mutation, Store, now_ms};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
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
    /// The counters as last written to the store: a commit persists [`Core::counters`]
    /// whenever they differ, so an allocated handle is never reused after a restart.
    persisted_counters: Counters,
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
    /// Force writing the counters. Not needed: every commit persists counters that changed.
    pub counters: bool,
    /// Pane runtimes of holders launched while building this transaction. When the
    /// transaction is dropped uncommitted or its commit fails, their panes never enter the
    /// model, so their holders are told to close (a holder exits only once its exit is
    /// acknowledged; an unattached one would live forever).
    pub spawned: Spawned,
    /// A UI convenience (focus, unread marks): if storage is unavailable it is applied in
    /// memory only, flagged ephemeral and lost on restart (02 §4a). Never set for anything a
    /// restart or another client must see (interactions, tasks, layout).
    pub ephemeral: bool,
}

/// See [`Tx::spawned`].
#[derive(Default)]
pub struct Spawned(Vec<Arc<PaneRt>>);

impl Spawned {
    pub fn push(&mut self, rt: Arc<PaneRt>) {
        self.0.push(rt);
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// The panes were committed: keep them running.
    fn disarm(&mut self) {
        self.0.clear();
    }
}

impl Drop for Spawned {
    fn drop(&mut self) {
        for rt in self.0.drain(..) {
            tracing::warn!(pane = %rt.id, "pane not committed; closing its holder");
            rt.send(PaneCmd::Close);
        }
    }
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
        if matches!(t.status.as_str(), "archived" | "forgotten") {
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
            persisted_counters: counters.clone(),
            counters,
            notifications: Vec::new(),
            ephemeral_hit: None,
        })
    }

    /// Persist then apply. Returns the committed events. Private: callers go through
    /// [`crate::Server::commit`] / [`crate::Server::commit_storage`].
    fn apply(&mut self, mut tx: Tx) -> Result<Vec<Event>> {
        // Dropped (closing the holders) unless the commit succeeds.
        let mut spawned = std::mem::take(&mut tx.spawned);
        // Handles allocated since the last write (any call site, flagged or not) are written
        // with this transaction: a restart must never hand out the same handle twice.
        let counters_changed = tx.counters || self.counters != self.persisted_counters;
        if counters_changed {
            tx.m.kv(
                "server",
                "counters",
                Some(serde_json::to_string(&self.counters)?),
            );
        }
        let events = match self.store.commit(std::mem::take(&mut tx.m)) {
            Ok(e) => {
                if counters_changed {
                    self.persisted_counters = self.counters.clone();
                }
                e
            }
            // Pure UI convenience while storage is down: keep it in memory, emit no event.
            Err(e) if tx.ephemeral => {
                self.ephemeral_hit = Some(format!("storage unavailable: {e:#}"));
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        spawned.disarm();
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
            if matches!(t.status.as_str(), "archived" | "forgotten") {
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

// ---- the commit path -------------------------------------------------------------------------

impl crate::Server {
    /// Commit a transaction, publish its events and bump the model revision.
    pub fn commit(&self, core: &mut Core, tx: Tx) -> Result<Vec<Event>> {
        self.commit_with(core, tx, true)
    }

    /// Commit a write that changes nothing clients render from the model (kv rows, holder
    /// records, VT snapshots, history rows, a pane's size): like [`Self::commit`] (events are
    /// published, storage failure enters degraded mode), but the model revision is bumped only
    /// when the degraded state changes. A busy pane's snapshots and resizes then do not make
    /// every client re-fetch the model.
    pub fn commit_storage(&self, core: &mut Core, tx: Tx) -> Result<Vec<Event>> {
        self.commit_with(core, tx, false)
    }

    fn commit_with(&self, core: &mut Core, tx: Tx, bump: bool) -> Result<Vec<Event>> {
        match core.apply(tx) {
            Ok(events) => {
                if let Some(msg) = core.ephemeral_hit.take() {
                    // The store refused the write and the transaction was a UI convenience
                    // (focus, unread marks): applied in memory only (02 §4a).
                    self.hardening.note_ephemeral();
                    self.enter_degraded(core, msg);
                    return Ok(events);
                }
                let was_degraded = self.degraded.lock().unwrap().take().is_some();
                core.model.degraded = None;
                self.hardening.recovered();
                for e in &events {
                    let _ = self.events.send(Arc::new(e.clone()));
                }
                if bump || was_degraded {
                    self.bump_model();
                }
                Ok(events)
            }
            Err(e) => {
                self.enter_degraded(core, format!("storage unavailable: {e:#}"));
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod commit_tests {
    use super::*;
    use crate::{Server, ServerOpts, paths::Paths};

    fn server(dir: &std::path::Path) -> Arc<Server> {
        let paths = Paths {
            session: "t".into(),
            runtime: dir.join("run"),
            state: dir.join("state"),
        };
        let opts = ServerOpts {
            session: "t".into(),
            machine: "testbox".into(),
            bin: "/bin/false".into(),
            hold_args: vec![],
            default_shell: None,
            env: vec![],
            shims: false,
            gateway: None,
        };
        Server::new(paths, opts).unwrap()
    }

    #[test]
    fn counters_changed_without_the_flag_are_persisted() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(dir.path());
        {
            let mut c = s.core.lock().unwrap();
            // A call site that allocates a handle and forgets `tx.counters = true`.
            let h = c.next_task_handle();
            assert_eq!(h, "k1");
            let mut tx = Tx::new();
            tx.m.kv("test", "x", Some("1".into()));
            s.commit(&mut c, tx).unwrap();
        }
        let stored: Counters = s.with_core(|c| {
            serde_json::from_str(&c.store.kv_get("server", "counters").unwrap().unwrap()).unwrap()
        });
        assert_eq!(stored.task, 1);
        // A restart continues after it.
        drop(s);
        let s = server(dir.path());
        assert_eq!(s.with_core(|c| c.next_task_handle()), "k2");
    }

    #[test]
    fn unchanged_counters_are_not_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(dir.path());
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv("test", "x", Some("1".into()));
        s.commit(&mut c, tx).unwrap();
        assert!(c.store.kv_get("server", "counters").unwrap().is_none());
    }

    #[test]
    fn storage_commits_do_not_bump_the_model() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(dir.path());
        let before = *s.model_rev.borrow();
        let mut c = s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv("test", "x", Some("1".into()));
        s.commit_storage(&mut c, tx).unwrap();
        assert_eq!(*s.model_rev.borrow(), before);
        let mut tx = Tx::new();
        tx.m.kv("test", "x", Some("2".into()));
        s.commit(&mut c, tx).unwrap();
        assert!(*s.model_rev.borrow() > before);
    }

    #[test]
    fn holders_of_an_uncommitted_transaction_are_closed() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(dir.path());
        // Dropped without a commit.
        let (rt, mut rx) = PaneRt::new("p-dropped", 80, 24);
        let mut tx = Tx::new();
        tx.spawned.push(rt);
        drop(tx);
        assert!(matches!(rx.try_recv(), Ok(PaneCmd::Close)));
        // A commit the store refuses.
        let (rt, mut rx) = PaneRt::new("p-refused", 80, 24);
        let mut c = s.core.lock().unwrap();
        c.store.set_query_only(true).unwrap();
        let mut tx = Tx::new();
        tx.m.kv("test", "x", Some("1".into()));
        tx.spawned.push(rt);
        assert!(s.commit(&mut c, tx).is_err());
        assert!(matches!(rx.try_recv(), Ok(PaneCmd::Close)));
        // A commit that succeeds keeps the pane running.
        c.store.set_query_only(false).unwrap();
        let (rt, mut rx) = PaneRt::new("p-kept", 80, 24);
        let mut tx = Tx::new();
        tx.m.kv("test", "x", Some("2".into()));
        tx.spawned.push(rt);
        s.commit(&mut c, tx).unwrap();
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn a_refused_pane_removal_keeps_the_pane_and_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let s = server(dir.path());
        {
            let mut c = s.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.ws(Workspace {
                id: "ws1".into(),
                handle: "w1".into(),
                name: None,
                auto_name: "w".into(),
                root_path: "/tmp".into(),
                task: None,
                order: 1.0,
                branch: None,
            });
            tx.tab(Tab {
                id: "tab1".into(),
                handle: "w1:t1".into(),
                workspace: "ws1".into(),
                title: None,
                number: 1,
                layout: LayoutNode::Leaf { pane: "p1".into() },
                focused_pane: Some("p1".into()),
                zoomed_pane: None,
                order: 1.0,
                floating: vec![],
                floats_hidden: false,
            });
            tx.pane(Pane {
                id: "p1".into(),
                handle: "w1:p1".into(),
                tab: "tab1".into(),
                workspace: "ws1".into(),
                title: None,
                auto_title: "sh".into(),
                cwd: None,
                cols: 80,
                rows: 24,
                child_pid: None,
                fg_cmdline: vec![],
                exited: true,
                exit_code: Some(0),
                unread: false,
                marked_unread: false,
                pinned: false,
                created_by: "user".into(),
                recovered: None,
                isolation: Isolation::default(),
                browser: None,
            });
            s.commit(&mut c, tx).unwrap();
        }
        s.with_core(|c| c.store.set_query_only(true).unwrap());
        s.pane_ended("p1", "exited:0");
        // Storage refused: the pane stays, the server is degraded, the removal is pending.
        assert!(s.with_core(|c| c.pane("p1").is_some()));
        assert!(s.degraded.lock().unwrap().is_some());
        s.with_core(|c| c.store.set_query_only(false).unwrap());
        s.retry_pane_ends();
        assert!(s.with_core(|c| c.pane("p1").is_none()));
        assert!(s.with_core(|c| c.ws("ws1").is_none()));
        assert!(s.pending_pane_ends.lock().unwrap().is_empty());
    }
}
