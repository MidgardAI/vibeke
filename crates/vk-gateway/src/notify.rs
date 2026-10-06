//! Push triggers (spec 16 §7.8): server events → merged, visible Web Push per device.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use crate::Gateway;
use crate::api::normalize;
use crate::events::Fanout;

/// What needs the user, per host, keyed by interaction id or `run:<id>`.
#[derive(Default)]
struct Open {
    items: BTreeMap<String, Item>,
}

#[derive(Clone)]
struct Item {
    title: String,
    url: String,
    urgent: bool,
    /// Pane the item belongs to (share devices only see their own panes' items).
    pane: Option<String>,
}

pub async fn run(gw: Arc<Gateway>) {
    let mut rx = gw.hub.subscribe();
    let mut open = Open::default();
    // Pending "finished" checks: run id → debounce generation.
    let mut finished: HashMap<String, u64> = HashMap::new();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<(String, u64)>(64);
    let mut gen_counter = 0u64;
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(Fanout::Event(e)) => e,
                    Ok(Fanout::Reset) => { open.items.clear(); continue; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let subject = ev.get("subject").cloned().unwrap_or_default();
                let data = ev.get("data").cloned().unwrap_or_default();
                match ty {
                    "interaction.opened" => {
                        let kind = data.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                        if !matches!(kind, "approval" | "question" | "plan_review" | "Approval" | "Question" | "PlanReview") { continue; }
                        let Some(id) = subject.get("interaction").and_then(|i| i.as_str()) else { continue };
                        let title = describe_interaction(&gw, id).await;
                        open.items.insert(id.to_string(), Item { title, url: format!("#/i/{}/{id}", gw.keys.host_id()), urgent: true, pane: subject.get("pane").and_then(|p| p.as_str()).map(str::to_string) });
                        send(&gw, &open, false).await;
                    }
                    "interaction.decided" | "interaction.cancelled" | "interaction.expired" | "interaction.delivered" => {
                        if let Some(id) = subject.get("interaction").and_then(|i| i.as_str()) {
                            open.items.remove(id);
                        }
                    }
                    "interaction.updated" => {
                        if let Some(id) = subject.get("interaction").and_then(|i| i.as_str())
                            && let Ok(v) = gw.server.call("interaction.get", json!({"interaction": id})).await
                        {
                            let mut it = v.get("interaction").cloned().unwrap_or_default();
                            normalize(&mut it);
                            if it.get("status").and_then(|s| s.as_str()) != Some("open") {
                                open.items.remove(id);
                            }
                        }
                    }
                    "agent.state_changed" => {
                        let Some(run) = subject.get("run").and_then(|r| r.as_str()) else { continue };
                        let to = data.get("to").and_then(|t| t.as_str()).unwrap_or("");
                        let from = data.get("from").and_then(|t| t.as_str()).unwrap_or("");
                        match to {
                            "idle" if from == "working" => {
                                gen_counter += 1;
                                finished.insert(run.to_string(), gen_counter);
                                let (tx, run, g) = (done_tx.clone(), run.to_string(), gen_counter);
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(30)).await;
                                    let _ = tx.send((run, g)).await;
                                });
                            }
                            "working" => { finished.remove(run); open.items.remove(&format!("run:{run}")); }
                            "error" | "rate_limited" => {
                                let what = if to == "error" { "stopped with an error" } else { "is rate limited" };
                                let title = format!("{} {what}", describe_run(&gw, run).await);
                                open.items.insert(format!("run:{run}"), Item { title, url: format!("#/r/{}/{run}", gw.keys.host_id()), urgent: false, pane: subject.get("pane").and_then(|p| p.as_str()).map(str::to_string) });
                                send(&gw, &open, false).await;
                            }
                            _ => {}
                        }
                    }
                    "notification.created" => {
                        let urgency = data.get("urgency").and_then(|u| u.as_str()).unwrap_or("normal");
                        let kind = data.get("kind").and_then(|u| u.as_str()).unwrap_or("");
                        // Interactions already produce their own push.
                        if urgency == "low" || kind.starts_with("interaction") || kind.contains("approval") { continue; }
                        let title = data.get("title").and_then(|t| t.as_str()).unwrap_or("Vibeke").to_string();
                        let id = data.get("id").and_then(|i| i.as_str()).unwrap_or("n").to_string();
                        open.items.insert(format!("note:{id}"), Item { title, url: "#/inbox".into(), urgent: false, pane: subject.get("pane").and_then(|p| p.as_str()).map(str::to_string) });
                        send(&gw, &open, false).await;
                        open.items.remove(&format!("note:{id}"));
                    }
                    _ => {}
                }
            }
            Some((run, g)) = done_rx.recv() => {
                if finished.get(&run) != Some(&g) { continue; }
                finished.remove(&run);
                // Still idle and nothing open for it?
                let Ok(r) = gw.server.call("agent.get", json!({"target": run})).await else { continue };
                let mut r = r;
                normalize(&mut r);
                let idle = r.pointer("/run/execution/value").and_then(|v| v.as_str()) == Some("idle");
                let open_its = r.get("open_interactions").and_then(|v| v.as_u64()).unwrap_or(0);
                if idle && open_its == 0 {
                    let title = format!("{} finished", describe_run(&gw, &run).await);
                    let mut done = Open::default();
                    let pane = r.pointer("/run/pane").and_then(|p| p.as_str()).map(str::to_string);
                    done.items.insert(format!("run:{run}"), Item { title, url: format!("#/r/{}/{run}", gw.keys.host_id()), urgent: false, pane });
                    send(&gw, &done, true).await;
                }
            }
        }
    }
}

async fn describe_run(gw: &Gateway, run: &str) -> String {
    let Ok(list) = gw.server.call("agent.list", json!({})).await else {
        return "An agent".into();
    };
    let runs = list
        .get("runs")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let Some(r) = runs
        .iter()
        .find(|r| r.get("id").and_then(|i| i.as_str()) == Some(run))
    else {
        return "An agent".into();
    };
    let harness = r.get("harness").and_then(|h| h.as_str()).unwrap_or("agent");
    let ws = r
        .get("workspace")
        .and_then(|w| w.get("name").or(Some(w)))
        .and_then(|w| w.as_str())
        .unwrap_or("");
    let name = capitalize(harness);
    if ws.is_empty() {
        name
    } else {
        format!("{name} · {ws}")
    }
}

async fn describe_interaction(gw: &Gateway, id: &str) -> String {
    let Ok(v) = gw
        .server
        .call("interaction.get", json!({"interaction": id}))
        .await
    else {
        return "An agent needs you".into();
    };
    let mut it = v.get("interaction").cloned().unwrap_or_default();
    normalize(&mut it);
    let who = match it.get("run").and_then(|r| r.as_str()) {
        Some(run) => describe_run(gw, run).await,
        None => "An agent".into(),
    };
    let what = match it.get("kind").and_then(|k| k.as_str()) {
        Some("approval") => match it.pointer("/action/command").and_then(|c| c.as_str()) {
            Some(cmd) => format!("wants to run `{}`", truncate(cmd, 80)),
            None => match it.pointer("/action/tool").and_then(|t| t.as_str()) {
                Some(t) => format!("wants to use {t}"),
                None => "needs approval".into(),
            },
        },
        Some("question") => "has a question".into(),
        Some("plan_review") => "wants a plan reviewed".into(),
        _ => "needs you".into(),
    };
    format!("{who} {what}")
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.into()
    } else {
        format!("{}…", s.chars().take(n).collect::<String>())
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    c.next()
        .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
        .unwrap_or_default()
}

/// Build the per-host merged payload for one device's privacy level.
fn payload(gw: &Gateway, open: &Open, privacy: &str) -> Value {
    let n = open.items.len();
    let host = &gw.host_name;
    let first = open.items.values().next();
    let (title, body) = match (privacy, n, first) {
        ("minimal", _, _) => (
            "Vibeke".to_string(),
            if n == 1 {
                "1 agent needs you".into()
            } else {
                format!("{n} agents need you")
            },
        ),
        (_, 1, Some(it)) => {
            let t = if privacy == "full" {
                vk_redact::redact(&it.title).to_string()
            } else {
                strip_command(&it.title)
            };
            (t, host.clone())
        }
        _ => (format!("{n} agents need you"), host.clone()),
    };
    let url = if n == 1 {
        first.map(|i| i.url.clone()).unwrap_or_default()
    } else {
        "#/inbox".into()
    };
    json!({"title": title, "body": body, "tag": format!("vibeke:{}", gw.keys.host_id()), "url": url, "host": gw.keys.host_id(), "count": n, "renotify": true})
}

/// `summary` level: keep "Codex · samplehub wants to run" and drop the command itself.
fn strip_command(t: &str) -> String {
    match t.find('`') {
        Some(i) => {
            t[..i]
                .trim_end()
                .trim_end_matches(" wants to run")
                .to_string()
                + " needs approval"
        }
        None => t.to_string(),
    }
}

/// The user typed in a TUI (on any machine) within this window: they're at the desk, so the
/// phone stays quiet (spec 12 presence-aware notifications; server X3).
const AT_DESK_MS: i64 = 120_000;

async fn user_at_desk(gw: &Gateway) -> bool {
    let Ok(v) = gw.server.call("client.list", json!({})).await else {
        return false;
    };
    let last = v
        .get("user_last_input_ms")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    last > 0 && now - last < AT_DESK_MS
}

async fn send(gw: &Arc<Gateway>, open: &Open, is_done: bool) {
    if open.items.is_empty() || gw.dnd() || user_at_desk(gw).await {
        return;
    }
    // Pane → workspace, fetched only if a limited (share) device needs it.
    let mut pane_ws: Option<Vec<(String, Option<String>)>> = None;
    for d in gw.devices() {
        if d.push.is_empty()
            || d.vapid_private.is_none()
            || gw.is_visible(&d.id)
            || d.expired()
            || d.kind == "handoff"
        {
            continue;
        }
        let visible = match crate::api::Allowed::of(&d) {
            None => Open {
                items: open.items.clone(),
            },
            Some(a) => {
                if pane_ws.is_none() {
                    let snap = gw
                        .server
                        .call("session.snapshot", json!({}))
                        .await
                        .unwrap_or_default();
                    pane_ws = Some(
                        snap.get("panes")
                            .and_then(|v| v.as_array())
                            .into_iter()
                            .flatten()
                            .filter_map(|p| {
                                Some((
                                    p.get("id")?.as_str()?.to_string(),
                                    p.get("workspace")
                                        .and_then(|w| w.as_str())
                                        .map(str::to_string),
                                ))
                            })
                            .collect(),
                    );
                }
                let map = pane_ws.as_ref().expect("fetched");
                let items = open
                    .items
                    .iter()
                    .filter(|(_, it)| {
                        it.pane.as_deref().is_some_and(|p| {
                            let ws = map
                                .iter()
                                .find(|(id, _)| id == p)
                                .and_then(|(_, w)| w.as_deref());
                            a.pane_ok(p, ws)
                        })
                    })
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                Open { items }
            }
        };
        if visible.items.is_empty() {
            continue;
        }
        let urgent = visible.items.values().any(|i| i.urgent);
        if is_done && !d.prefs.notify_done {
            continue;
        }
        if !is_done && urgent && !d.prefs.notify_input {
            continue;
        }
        let p = payload(gw, &visible, &d.prefs.privacy);
        let gw = gw.clone();
        tokio::spawn(async move {
            // push_to re-checks that the device still exists and hasn't expired.
            gw.push_to(&d.id, &p, if urgent { "high" } else { "normal" })
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_hides_command() {
        assert_eq!(
            strip_command("Codex · samplehub wants to run `rm -rf /`"),
            "Codex · samplehub needs approval"
        );
        assert_eq!(
            strip_command("Claude · x has a question"),
            "Claude · x has a question"
        );
    }
}
