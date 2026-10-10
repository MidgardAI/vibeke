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
    /// The title at the `summary` privacy level, when stripping the command is not enough.
    summary: Option<String>,
    url: String,
    urgent: bool,
    /// Pane the item belongs to (share devices only see their own panes' items).
    pane: Option<String>,
}

impl Item {
    fn new(title: String, url: String, urgent: bool, pane: Option<String>) -> Item {
        Item {
            title,
            summary: None,
            url,
            urgent,
            pane,
        }
    }
}

/// Which kind of push a send is: it picks the device pref that allows it and the tag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Push {
    /// Open items that need the user (`notify_input`); one merged notification per host.
    NeedsYou,
    /// A run finished (`notify_done`); replaces the host's notification.
    Done,
    /// An idle run's prompt cache expires soon (`notify_cache_cold`); its own notification.
    CacheCold,
}

/// How long a harness keeps an idle conversation's prompt cache. `None`: no notice.
pub fn cache_ttl(harness: &str) -> Option<Duration> {
    match harness {
        "claude" | "codex" => Some(Duration::from_secs(300)),
        _ => None,
    }
}

/// The cache notice comes this long before the cache expires.
const CACHE_LEAD: Duration = Duration::from_secs(45);

/// When to send the cache notice after a run went idle.
pub fn cache_notice_after(harness: &str) -> Option<Duration> {
    cache_ttl(harness).map(|ttl| ttl.saturating_sub(CACHE_LEAD))
}

/// Per device, the open item keys its current needs-you notification shows. When none of them
/// is open any more (answered on another device or at the desk), a device whose app can close
/// notifications gets a `clear` push.
#[derive(Default)]
struct Shown {
    by_device: HashMap<String, Vec<String>>,
}

impl Shown {
    /// Record what a send put on each device's screen.
    fn sent(&mut self, kind: Push, sent: Vec<(String, Vec<String>)>) {
        for (device, keys) in sent {
            match kind {
                // A note shares the tag but is no needs-you item: never cleared.
                Push::NeedsYou if keys.iter().all(|k| !k.starts_with("note:")) => {
                    self.by_device.insert(device, keys);
                }
                Push::NeedsYou | Push::Done => {
                    self.by_device.remove(&device);
                }
                Push::CacheCold => {}
            }
        }
    }

    /// Devices whose notification shows only items that are no longer open.
    fn resolved(&mut self, open: &Open) -> Vec<String> {
        let gone: Vec<String> = self
            .by_device
            .iter()
            .filter(|(_, keys)| keys.iter().all(|k| !open.items.contains_key(k)))
            .map(|(d, _)| d.clone())
            .collect();
        for d in &gone {
            self.by_device.remove(d);
        }
        gone
    }
}

/// The `clear` push: closes this host's notification on a device that supports it.
fn clear_payload(host_id: &str) -> Value {
    json!({"kind": "clear", "tag": format!("vibeke:{host_id}"), "host": host_id})
}

async fn send_clears(gw: &Arc<Gateway>, shown: &mut Shown, open: &Open) {
    for device in shown.resolved(open) {
        let Some(d) = gw.device(&device) else {
            continue;
        };
        if !d.supports_clear || d.push.is_empty() || d.vapid_private.is_none() {
            continue;
        }
        let gw = gw.clone();
        let p = clear_payload(&gw.keys.host_id());
        tokio::spawn(async move {
            gw.push_to(&d.id, &p, "normal").await;
        });
    }
}

fn str_at<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}

pub async fn run(gw: Arc<Gateway>) {
    let mut rx = gw.hub.subscribe();
    let mut open = Open::default();
    let mut shown = Shown::default();
    // Pending "finished" checks: run id → debounce generation.
    let mut finished: HashMap<String, u64> = HashMap::new();
    let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<(String, u64)>(64);
    // Pending prompt-cache notices: run id → generation (one per idle period).
    let mut cold: HashMap<String, u64> = HashMap::new();
    let (cold_tx, mut cold_rx) = tokio::sync::mpsc::channel::<(String, u64)>(64);
    let mut gen_counter = 0u64;
    let host = gw.keys.host_id();
    loop {
        tokio::select! {
            ev = rx.recv() => {
                let ev = match ev {
                    Ok(Fanout::Event(e)) => e,
                    // Events were lost: what is open is unknown, so nothing is cleared either.
                    Ok(Fanout::Reset) => { open.items.clear(); shown = Shown::default(); continue; }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                };
                let ty = ev.get("type").and_then(|t| t.as_str()).unwrap_or("");
                let subject = ev.get("subject").cloned().unwrap_or_default();
                let data = ev.get("data").cloned().unwrap_or_default();
                let pane = str_at(&subject, "pane").map(str::to_string);
                let before = open.items.len();
                match ty {
                    "interaction.opened" => {
                        let kind = data.get("kind").and_then(|k| k.as_str()).unwrap_or("");
                        if !matches!(kind, "approval" | "question" | "plan_review" | "Approval" | "Question" | "PlanReview") { continue; }
                        let Some(id) = subject.get("interaction").and_then(|i| i.as_str()) else { continue };
                        let title = describe_interaction(&gw, id).await;
                        open.items.insert(id.to_string(), Item::new(title, format!("#/i/{host}/{id}"), true, pane));
                        let sent = send(&gw, &open, Push::NeedsYou).await;
                        shown.sent(Push::NeedsYou, sent);
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
                                let (tx, r, g) = (done_tx.clone(), run.to_string(), gen_counter);
                                tokio::spawn(async move {
                                    tokio::time::sleep(Duration::from_secs(30)).await;
                                    let _ = tx.send((r, g)).await;
                                });
                                // The prompt cache starts to age now; only looked up when a
                                // device wants the notice.
                                if gw.devices().iter().any(|d| d.prefs.notify_cache_cold)
                                    && let Ok(r) = gw.server.call("agent.get", json!({"target": run})).await
                                    && let Some(after) = r.pointer("/run/harness").and_then(|h| h.as_str()).and_then(cache_notice_after)
                                {
                                    cold.insert(run.to_string(), gen_counter);
                                    let (tx, r, g) = (cold_tx.clone(), run.to_string(), gen_counter);
                                    tokio::spawn(async move {
                                        tokio::time::sleep(after).await;
                                        let _ = tx.send((r, g)).await;
                                    });
                                }
                            }
                            "working" => {
                                finished.remove(run);
                                cold.remove(run);
                                open.items.remove(&format!("run:{run}"));
                            }
                            "error" | "rate_limited" => {
                                let what = if to == "error" { "stopped with an error" } else { "is rate limited" };
                                let title = format!("{} {what}", describe_run(&gw, run).await);
                                open.items.insert(format!("run:{run}"), Item::new(title, format!("#/r/{host}/{run}"), false, pane));
                                let sent = send(&gw, &open, Push::NeedsYou).await;
                                shown.sent(Push::NeedsYou, sent);
                            }
                            _ => {}
                        }
                    }
                    // A new turn or the end of the run: no cache notice for this idle period.
                    "agent.turn_started" | "agent.exited" | "agent.session_ended" => {
                        if let Some(run) = subject.get("run").and_then(|r| r.as_str()) {
                            cold.remove(run);
                        }
                    }
                    // A pane asks the user to approve one call (09 §3.2 "Approved calls"): the
                    // push opens the app's review screen for it. It carries no approve action;
                    // deciding needs a full-scope device and an explicit tap in the app. Share
                    // devices never see it (no pane on the item).
                    "auth.approval_requested" => {
                        let Some(id) = subject.get("request").and_then(|i| i.as_str()) else { continue };
                        let method = data.get("method").and_then(|m| m.as_str()).unwrap_or("");
                        let title = format!("A pane asks to {}", approval_verb(method));
                        open.items.insert(format!("approval:{id}"), Item::new(title, format!("#/approve/{host}/{id}"), true, None));
                        let sent = send(&gw, &open, Push::NeedsYou).await;
                        shown.sent(Push::NeedsYou, sent);
                    }
                    "auth.approval_granted" | "auth.approval_denied" | "auth.approval_withdrawn" => {
                        if let Some(id) = subject.get("request").and_then(|i| i.as_str()) {
                            open.items.remove(&format!("approval:{id}"));
                        }
                    }
                    // A goal's plan waits for approval (12 "Goal -> plan -> tasks"). Goals span
                    // repositories, so share devices never see it (no pane on the item).
                    "goal.planned" => {
                        let Some(id) = subject.get("goal").and_then(|i| i.as_str()) else { continue };
                        let Ok(g) = gw.server.call("goal.get", json!({"goal": id})).await else { continue };
                        if g.pointer("/goal/state").and_then(|s| s.as_str()) != Some("planned") { continue; }
                        let name = g.pointer("/goal/title").and_then(|s| s.as_str()).unwrap_or("a goal");
                        let mut item = Item::new(format!("The plan for {} waits for approval", truncate(name, 80)), format!("#/g/{host}/{id}"), true, None);
                        item.summary = Some("A plan waits for approval".into());
                        open.items.insert(format!("goal:{id}"), item);
                        let sent = send(&gw, &open, Push::NeedsYou).await;
                        shown.sent(Push::NeedsYou, sent);
                    }
                    "goal.approved" | "goal.cancelled" | "goal.finished" | "goal.planning_started" => {
                        if let Some(id) = subject.get("goal").and_then(|i| i.as_str()) {
                            open.items.remove(&format!("goal:{id}"));
                        }
                    }
                    "notification.created" => {
                        let urgency = data.get("urgency").and_then(|u| u.as_str()).unwrap_or("normal");
                        let kind = data.get("kind").and_then(|u| u.as_str()).unwrap_or("");
                        // Interactions already produce their own push.
                        if urgency == "low" || kind.starts_with("interaction") || kind.contains("approval") { continue; }
                        // An approval request pushes from its own event (above).
                        if kind == "auth.approve" && urgency == "high" { continue; }
                        let title = data.get("title").and_then(|t| t.as_str()).unwrap_or("Vibeke").to_string();
                        let id = data.get("id").and_then(|i| i.as_str()).unwrap_or("n").to_string();
                        // Incoming handoffs open the host's handoff list in the app.
                        let url = match kind {
                            "handoff" => format!("#/handoffs/{host}"),
                            // A standing approval was used: the host's approvals screen.
                            "auth.approve" => format!("#/approve/{host}"),
                            _ => "#/inbox".into(),
                        };
                        open.items.insert(format!("note:{id}"), Item::new(title, url, false, pane));
                        let sent = send(&gw, &open, Push::NeedsYou).await;
                        shown.sent(Push::NeedsYou, sent);
                        open.items.remove(&format!("note:{id}"));
                        continue;
                    }
                    _ => {}
                }
                if open.items.len() < before {
                    send_clears(&gw, &mut shown, &open).await;
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
                    done.items.insert(format!("run:{run}"), Item::new(title, format!("#/r/{host}/{run}"), false, pane));
                    let sent = send(&gw, &done, Push::Done).await;
                    shown.sent(Push::Done, sent);
                }
            }
            Some((run, g)) = cold_rx.recv() => {
                if cold.get(&run) != Some(&g) { continue; }
                cold.remove(&run);
                let Ok(r) = gw.server.call("agent.get", json!({"target": run})).await else { continue };
                let mut r = r;
                normalize(&mut r);
                let idle = r.pointer("/run/execution/value").and_then(|v| v.as_str()) == Some("idle");
                let open_its = r.get("open_interactions").and_then(|v| v.as_u64()).unwrap_or(0);
                // A run waiting on the user already has its own notification.
                if idle && open_its == 0 {
                    let title = format!("{}: prompt cache expires soon", describe_run(&gw, &run).await);
                    let mut note = Open::default();
                    let pane = r.pointer("/run/pane").and_then(|p| p.as_str()).map(str::to_string);
                    note.items.insert(format!("cache:{run}"), Item::new(title, format!("#/r/{host}/{run}"), false, pane));
                    send(&gw, &note, Push::CacheCold).await;
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
        // A sandbox boundary request (push, copy out): its title says what.
        Some("approval") if crate::api::boundary_of(&it).is_some() => {
            let t = it.get("title").and_then(|t| t.as_str()).unwrap_or("");
            format!("asks the host: {}", truncate(t, 80))
        }
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

/// What an approved call does (`auth.approval_requested` data.method), for the push title.
fn approval_verb(method: &str) -> &'static str {
    match method {
        "handoff.send" => "send a handoff",
        "handoff.cancel" => "cancel a handoff",
        "gateway.call" => "redeem a peer invitation",
        "preview.declare" => "open a port to its browser",
        _ => "run a call",
    }
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
                it.summary
                    .clone()
                    .unwrap_or_else(|| strip_command(&it.title))
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

/// Push `open` to every device allowed to see it. Returns each device sent to, with the item
/// keys its notification shows.
async fn send(gw: &Arc<Gateway>, open: &Open, kind: Push) -> Vec<(String, Vec<String>)> {
    let mut sent = vec![];
    if open.items.is_empty() || gw.dnd() || user_at_desk(gw).await {
        return sent;
    }
    // Pane → workspace, fetched only if a limited (share) device needs it.
    let mut pane_ws: Option<Vec<(String, Option<String>)>> = None;
    for d in gw.devices() {
        // Push is for the user's own devices only; a share device registered by an older
        // gateway keeps its stored subscription but is not sent anything.
        if d.kind != "device"
            || d.push.is_empty()
            || d.vapid_private.is_none()
            || gw.is_visible(&d.id)
            || d.expired()
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
        let wanted = match kind {
            Push::Done => d.prefs.notify_done,
            Push::CacheCold => d.prefs.notify_cache_cold,
            Push::NeedsYou => !urgent || d.prefs.notify_input,
        };
        if !wanted {
            continue;
        }
        let mut p = payload(gw, &visible, &d.prefs.privacy);
        if kind == Push::CacheCold
            && let Some(key) = visible.items.keys().next()
        {
            // Its own notification: it must not replace what needs the user.
            p["tag"] = format!("vibeke:{}:{key}", gw.keys.host_id()).into();
            p["renotify"] = false.into();
        }
        sent.push((d.id.clone(), visible.items.keys().cloned().collect()));
        let gw = gw.clone();
        tokio::spawn(async move {
            // push_to re-checks that the device still exists and hasn't expired.
            gw.push_to(&d.id, &p, if urgent { "high" } else { "normal" })
                .await;
        });
    }
    sent
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(keys: &[&str]) -> Open {
        let mut o = Open::default();
        for k in keys {
            o.items.insert(
                k.to_string(),
                Item::new("t".into(), "#/".into(), true, None),
            );
        }
        o
    }

    #[test]
    fn clear_follows_the_items_a_device_was_shown() {
        let mut shown = Shown::default();
        shown.sent(
            Push::NeedsYou,
            vec![
                ("phone".into(), vec!["i1".into(), "i2".into()]),
                ("tablet".into(), vec!["i1".into()]),
            ],
        );
        // i1 answered elsewhere: the tablet's notification only showed i1.
        assert_eq!(shown.resolved(&open(&["i2"])), vec!["tablet".to_string()]);
        // Asked again: nothing more until the phone's last item goes.
        assert!(shown.resolved(&open(&["i2"])).is_empty());
        assert_eq!(shown.resolved(&open(&[])), vec!["phone".to_string()]);
        assert!(shown.resolved(&open(&[])).is_empty());
    }

    #[test]
    fn other_notifications_on_the_host_tag_are_never_cleared() {
        let mut shown = Shown::default();
        shown.sent(Push::NeedsYou, vec![("phone".into(), vec!["i1".into()])]);
        // A "finished" push replaced the notification (same tag): keep it.
        shown.sent(Push::Done, vec![("phone".into(), vec!["run:r1".into()])]);
        assert!(shown.resolved(&open(&[])).is_empty());
        // A note merged into the notification: keep it too.
        shown.sent(
            Push::NeedsYou,
            vec![("phone".into(), vec!["i2".into(), "note:n1".into()])],
        );
        assert!(shown.resolved(&open(&[])).is_empty());
        // A cache notice has its own tag and changes nothing.
        shown.sent(Push::NeedsYou, vec![("phone".into(), vec!["i3".into()])]);
        shown.sent(
            Push::CacheCold,
            vec![("phone".into(), vec!["cache:r1".into()])],
        );
        assert_eq!(shown.resolved(&open(&[])), vec!["phone".to_string()]);
        assert_eq!(
            clear_payload("h1"),
            json!({"kind": "clear", "tag": "vibeke:h1", "host": "h1"})
        );
    }

    #[test]
    fn cache_notice_comes_before_the_harness_cache_expires() {
        for h in ["claude", "codex"] {
            assert_eq!(cache_ttl(h), Some(Duration::from_secs(300)), "{h}");
            let after = cache_notice_after(h).unwrap();
            assert!(after < Duration::from_secs(300) && after >= Duration::from_secs(200));
        }
        for h in ["pi", "opencode", "gemini", ""] {
            assert_eq!(cache_notice_after(h), None, "{h}");
        }
    }

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
