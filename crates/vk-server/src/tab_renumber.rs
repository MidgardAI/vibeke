//! `tab.renumber {workspace}` (08 §3: tab numbers are assigned at creation and change only on
//! an explicit `tab renumber`): the workspace's tabs get numbers 1..n in their current order,
//! with handles `<workspace>:t<n>` to match, and the next new tab gets n + 1. Pane handles
//! do not change. Event `tab.renumbered {tabs: [{tab, from, to}]}`. Full scope only.

use crate::Server;
use crate::api::{Ctx, R, internal, resolve_ws, s};
use crate::core::Tx;
use serde_json::{Value, json};
use std::sync::Arc;

pub const METHODS: &[(&str, bool)] = &[("tab.renumber", true)];

fn renumber(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let ws = resolve_ws(server, ctx, s(p, "workspace"))?;
    let mut c = server.core.lock().unwrap();
    let mut tabs: Vec<_> = c.tabs_of(&ws.id).into_iter().cloned().collect();
    tabs.sort_by(|a, b| a.order.total_cmp(&b.order));
    let mut tx = Tx::new();
    let mut changes = vec![];
    for (i, t) in tabs.iter_mut().enumerate() {
        let n = i as u32 + 1;
        if t.number != n {
            changes.push(json!({"tab": t.id, "from": t.number, "to": n}));
            t.number = n;
            t.handle = format!("{}:t{n}", ws.handle);
            tx.tab(t.clone());
        }
    }
    c.counters.tab.insert(ws.id.clone(), tabs.len() as u32);
    tx.counters = true;
    if !changes.is_empty() {
        tx.event(
            "tab.renumbered",
            json!({"workspace": ws.id}),
            json!({"tabs": changes}),
        );
    }
    let events = server.commit(&mut c, tx).map_err(internal)?;
    drop(c);
    Ok(json!({
        "workspace": ws.id,
        "tabs": tabs,
        "changed": changes.len(),
        "cursor": crate::api::cursor(server, events.last().map(|e| e.seq)),
    }))
}

/// Dispatch hook for `tab.renumber`.
pub fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    (method == "tab.renumber").then(|| renumber(server, ctx, p))
}
