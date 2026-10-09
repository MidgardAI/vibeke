//! Deadline-driven idle loop (spec 10 §1.3.1): instead of a fixed tick, every feature that needs
//! time arms a deadline from its own state (a toast expiring, the prefix timeout, an age label
//! about to change, a poll that is due, a retry after a lost request) and the run loop sleeps
//! until the earliest one. With nothing armed it sleeps until input or a server frame.
//!
//! The housekeeping itself stays in `App::on_tick`, which the run loop calls after every wakeup
//! (input, server frames and deadlines alike), so a condition that becomes due because of an
//! event is handled on that same wakeup; the deadlines only cover what time alone changes.

use std::time::{Duration, Instant};

/// Wake this long after a deadline, so checks written as `elapsed() > limit` see it as passed.
pub const SLACK: Duration = Duration::from_millis(1);
/// Never sleep less than this: a deadline the housekeeping could not clear does not spin.
pub const MIN_NAP: Duration = Duration::from_millis(5);

/// One armed deadline: what it is for, when, and whether it only needs a redraw (an age label,
/// a countdown, the clock) rather than a housekeeping step that sets `dirty` itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    pub what: &'static str,
    pub at: Instant,
    pub redraw: bool,
}

/// The deadlines armed for the current state (see `App::deadlines`).
#[derive(Debug, Default, Clone)]
pub struct Deadlines {
    items: Vec<Deadline>,
}

impl Deadlines {
    /// A housekeeping step due at `at` (a poll, an expiry, a retry).
    pub fn at(&mut self, what: &'static str, at: Instant) {
        self.items.push(Deadline {
            what,
            at,
            redraw: false,
        });
    }

    /// Something on screen changes at `at` without any state changing (repaint then).
    pub fn redraw(&mut self, what: &'static str, at: Instant) {
        self.items.push(Deadline {
            what,
            at,
            redraw: true,
        });
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn items(&self) -> &[Deadline] {
        &self.items
    }

    /// The earliest deadline.
    pub fn next(&self) -> Option<Instant> {
        self.items.iter().map(|d| d.at).min()
    }

    /// The earliest deadline armed for `what`.
    pub fn get(&self, what: &str) -> Option<Instant> {
        self.items
            .iter()
            .filter(|d| d.what == what)
            .map(|d| d.at)
            .min()
    }

    /// Names of everything armed (sorted, deduplicated), for tests and diagnostics.
    pub fn names(&self) -> Vec<&'static str> {
        let mut v: Vec<_> = self.items.iter().map(|d| d.what).collect();
        v.sort_unstable();
        v.dedup();
        v
    }

    /// A redraw-only deadline has passed.
    pub fn redraw_due(&self, now: Instant) -> bool {
        self.items.iter().any(|d| d.redraw && d.at <= now)
    }
}

/// When the run loop should wake for `next` (None: sleep until input or a server frame).
pub fn wake_at(next: Option<Instant>, now: Instant) -> Option<Instant> {
    next.map(|t| t.max(now + MIN_NAP) + SLACK)
}

/// Milliseconds until a label showing `elapsed_ms` in the `s` / `m` / `h` / `d` format of
/// `inbox::fmt_age` (and the sidebar's `working · 12s`) shows a different value.
pub fn age_change_in(elapsed_ms: i64) -> u64 {
    let e = elapsed_ms.max(0);
    let unit = if e < 60_000 {
        1_000
    } else if e < 3_600_000 {
        60_000
    } else if e < 86_400_000 {
        3_600_000
    } else {
        86_400_000
    };
    (unit - e % unit) as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{Mode, Popup, Toast, test_app, test_run};
    use serde_json::{Value, json};
    use tokio::sync::mpsc::UnboundedReceiver;
    use vk_proto::render::{ClientFrame, ServerFrame};

    fn drain(rx: &mut UnboundedReceiver<ClientFrame>) -> Vec<(u64, Value)> {
        let mut v = Vec::new();
        while let Ok(f) = rx.try_recv() {
            if let ClientFrame::Command { req, json } = f {
                v.push((req, serde_json::from_str(&json).unwrap()));
            }
        }
        v
    }

    fn methods(cmds: &[(u64, Value)]) -> Vec<String> {
        cmds.iter()
            .filter_map(|(_, v)| v["method"].as_str().map(str::to_string))
            .collect()
    }

    fn answer(app: &mut crate::app::App, mi: usize, req: u64, result: Value) {
        let resp = json!({"jsonrpc": "2.0", "id": req, "result": result});
        app.on_frame(
            mi,
            ServerFrame::CommandResult {
                req,
                json: resp.to_string(),
            },
        );
    }

    /// A connected App whose server pushes events, caught up and with its device list fetched:
    /// what a TUI attached to an idle session looks like.
    fn idle_app() -> (crate::app::App, Vec<UnboundedReceiver<ClientFrame>>) {
        let (mut app, mut rxs) = test_app(1);
        app.machines[0].features = vec![crate::push::FEATURE.to_string()];
        app.on_connected(0);
        app.on_tick();
        let cmds = drain(&mut rxs[0]);
        for (req, v) in &cmds {
            match v["method"].as_str() {
                Some("events.read") => answer(&mut app, 0, *req, json!({"events": [], "next": 0})),
                Some("client.list") => {
                    answer(&mut app, 0, *req, json!({"clients": [], "devices": []}))
                }
                _ => {}
            }
        }
        app.on_tick();
        drain(&mut rxs[0]);
        app.toasts.clear();
        app.mode = Mode::Normal;
        (app, rxs)
    }

    #[test]
    fn idle_app_arms_no_deadline() {
        let (app, _rxs) = idle_app();
        let now = Instant::now();
        let d = app.deadlines(now);
        assert!(d.is_empty(), "idle App armed {:?}", d.items());
        assert_eq!(app.next_deadline(now), None);
        assert_eq!(wake_at(app.next_deadline(now), now), None);
        // A disconnected machine arms nothing either.
        let (mut app, _rxs) = test_app(1);
        app.machines[0].tx = None;
        assert!(app.deadlines(Instant::now()).is_empty());
    }

    #[test]
    fn idle_runs_do_not_animate_working_ones_tick_their_age() {
        let (mut app, _rxs) = idle_app();
        // An idle agent shows a static "idle": nothing to repaint.
        app.machines[0]
            .model
            .runs
            .push(test_run("r1", "p1", "claude"));
        assert!(app.deadlines(Instant::now()).is_empty());
        // A working one shows "working · Ns": repaint when the seconds change.
        let mut r = test_run("r2", "p2", "claude");
        r.execution.value = vk_proto::model::Execution::Working;
        r.execution.since_ms = crate::drafts::now_ms() - 1_300;
        app.machines[0].model.runs.push(r);
        let now = Instant::now();
        let d = app.deadlines(now);
        assert_eq!(d.names(), vec!["ages"]);
        let at = d.get("ages").unwrap();
        assert!(
            at > now && at <= now + Duration::from_millis(1_000),
            "{at:?}"
        );
        assert!(d.redraw_due(at) && !d.redraw_due(now));
    }

    #[test]
    fn age_labels_change_at_their_unit() {
        assert_eq!(age_change_in(0), 1_000);
        assert_eq!(age_change_in(1_300), 700);
        assert_eq!(age_change_in(59_999), 1);
        assert_eq!(age_change_in(60_000), 60_000);
        assert_eq!(age_change_in(90_000), 30_000);
        assert_eq!(age_change_in(3_600_000 + 5), 3_600_000 - 5);
        assert_eq!(age_change_in(86_400_000), 86_400_000);
        assert_eq!(age_change_in(-5), 1_000);
    }

    #[test]
    fn age_popups_repaint_each_second_while_open() {
        let (mut app, _rxs) = idle_app();
        for p in [Popup::Desk, Popup::Gallery] {
            app.mode = Mode::Popup(p);
            let now = Instant::now();
            let at = app.deadlines(now).get("ages").expect("ages armed");
            assert!(at > now && at <= now + Duration::from_secs(1));
        }
        app.mode = Mode::Popup(Popup::Message {
            title: "t".into(),
            body: String::new(),
        });
        assert!(app.deadlines(Instant::now()).is_empty());
    }

    #[test]
    fn toast_arms_its_expiry_and_on_tick_clears_it() {
        let (mut app, _rxs) = idle_app();
        let now = Instant::now();
        let until = now + Duration::from_millis(30);
        app.toasts.push(Toast {
            text: "hi".into(),
            until,
            pane: None,
        });
        app.toasts.push(Toast {
            text: "later".into(),
            until: now + Duration::from_secs(9),
            pane: None,
        });
        assert_eq!(app.next_deadline(now), Some(until));
        assert_eq!(app.deadlines(now).names(), vec!["toast"]);
        std::thread::sleep(Duration::from_millis(40));
        app.on_tick();
        assert_eq!(app.toasts.len(), 1);
        assert_eq!(
            app.next_deadline(Instant::now()),
            Some(now + Duration::from_secs(9))
        );
    }

    #[test]
    fn prefix_mode_arms_its_timeout() {
        let (mut app, _rxs) = idle_app();
        app.keymap.prefix_timeout_ms = 20;
        app.keymap.menu_ms = None;
        let at = Instant::now();
        app.mode = Mode::Prefix(crate::app::PrefixState {
            since: at,
            seq: vec![],
            menu: false,
        });
        let d = app.deadlines(at);
        assert_eq!(d.names(), vec!["prefix"]);
        assert_eq!(d.get("prefix"), Some(at + Duration::from_millis(20)));
        std::thread::sleep(wake_at(d.next(), at).unwrap() - at);
        app.on_tick();
        assert!(matches!(app.mode, Mode::Normal));
        assert!(app.deadlines(Instant::now()).is_empty());
    }

    #[test]
    fn confirm_overlay_arms_countdown_and_expiry() {
        let (mut app, _rxs) = idle_app();
        let now = Instant::now();
        let deadline = now + Duration::from_millis(2_400);
        app.gateway.push(crate::gateway::Confirm {
            machine: 0,
            id: "c1".into(),
            title: "t".into(),
            body: String::new(),
            options: vec![("yes".into(), "Yes".into())],
            deadline,
            sel: 0,
            shown_at: now,
        });
        let d = app.deadlines(now);
        assert_eq!(d.names(), vec!["confirm.countdown", "confirm.expire"]);
        // "expires in 2s" turns into "1s" 400 ms from now.
        assert_eq!(
            d.get("confirm.countdown"),
            Some(now + Duration::from_millis(400))
        );
        assert_eq!(d.get("confirm.expire"), Some(deadline));
        assert!(d.redraw_due(now + Duration::from_millis(400)));
    }

    #[test]
    fn gateway_polls_arm_their_own_intervals() {
        // An older server (no event push): events.read every second, client.list every 15 s.
        let (mut app, mut rxs) = test_app(1);
        app.on_connected(0);
        let now = Instant::now();
        let d = app.deadlines(now);
        assert!(d.get("gateway.events").is_some_and(|t| t <= now));
        assert!(d.get("gateway.list").is_some_and(|t| t <= now));
        app.on_tick();
        let cmds = drain(&mut rxs[0]);
        assert!(methods(&cmds).contains(&"events.read".to_string()));
        let t0 = Instant::now();
        // In flight: re-sent only if lost (10 s without an answer).
        let d = app.deadlines(t0);
        let ev = d.get("gateway.events").unwrap();
        assert!(ev >= t0 + Duration::from_secs(9) && ev <= t0 + Duration::from_secs(11));
        for (req, v) in &cmds {
            match v["method"].as_str() {
                Some("events.read") => answer(&mut app, 0, *req, json!({"events": [], "next": 0})),
                Some("client.list") => answer(&mut app, 0, *req, json!({"devices": []})),
                _ => {}
            }
        }
        let t1 = Instant::now();
        let d = app.deadlines(t1);
        let ev = d.get("gateway.events").unwrap();
        assert!(ev > t1 && ev <= t1 + Duration::from_secs(1), "{ev:?}");
        let li = d.get("gateway.list").unwrap();
        assert!(li > t1 + Duration::from_secs(13) && li <= t1 + Duration::from_secs(15));
        // After the deadline the tick polls again.
        std::thread::sleep(wake_at(Some(ev), Instant::now()).unwrap() - Instant::now());
        app.on_tick();
        assert!(methods(&drain(&mut rxs[0])).contains(&"events.read".to_string()));
    }

    #[test]
    fn pushed_client_events_refresh_the_list_without_a_timer() {
        let (mut app, mut rxs) = idle_app();
        crate::gateway::refresh_list(&mut app, 0);
        let cmds = drain(&mut rxs[0]);
        assert_eq!(methods(&cmds), vec!["client.list".to_string()]);
        // In flight: only the lost-request retry is armed.
        let now = Instant::now();
        let li = app.deadlines(now).get("gateway.list").unwrap();
        assert!(li > now + Duration::from_secs(9));
        answer(&mut app, 0, cmds[0].0, json!({"devices": []}));
        assert!(app.deadlines(Instant::now()).is_empty());
    }

    #[test]
    fn inbox_refresh_arms_only_while_open_and_stale() {
        let (mut app, mut rxs) = idle_app();
        app.inbox.stale = true;
        assert!(
            app.deadlines(Instant::now()).is_empty(),
            "closed: no refresh"
        );
        crate::inbox::open(&mut app);
        drain(&mut rxs[0]);
        let opened = app.inbox.last_fetch.unwrap();
        app.inbox.stale = true;
        let d = app.deadlines(Instant::now());
        assert_eq!(
            d.get("inbox"),
            Some(opened + crate::inbox::REFRESH_MIN),
            "{:?}",
            d.items()
        );
        app.inbox.stale = false;
        assert_eq!(app.deadlines(Instant::now()).get("inbox"), None);
    }

    #[test]
    fn assist_wait_arms_its_poll() {
        let (mut app, _rxs) = idle_app();
        let now = Instant::now();
        use crate::assist::{Op, Origin, Phase};
        crate::assist::start(&mut app, 0, Op::PaneTitle, json!({}), Origin::Palette, None);
        let f = app.assist.as_mut().unwrap();
        f.phase = Phase::Generating;
        assert!(
            app.deadlines(now).get("assist").is_none(),
            "generating: no poll"
        );
        let f = app.assist.as_mut().unwrap();
        f.phase = Phase::Waiting {
            request: "rq1".into(),
            auto: false,
        };
        f.last_poll = Some(now);
        app.mode = Mode::Normal;
        let d = app.deadlines(now);
        assert_eq!(d.names(), vec!["assist"]);
        assert_eq!(d.get("assist"), Some(now + Duration::from_millis(1000)));
    }

    #[test]
    fn task_view_polls_only_watched_messages() {
        let (mut app, _rxs) = idle_app();
        let now = Instant::now();
        crate::tasks::open_task(&mut app, 0, "t1");
        assert!(app.deadlines(now).get("tasks").is_none(), "nothing sending");
        let v = app.task_view.as_mut().unwrap();
        v.watch.insert("msg1".into());
        v.last_poll = Some(now);
        assert_eq!(
            app.deadlines(now).get("tasks"),
            Some(now + Duration::from_secs(1))
        );
        app.mode = Mode::Normal;
        assert!(app.deadlines(now).get("tasks").is_none(), "view closed");
    }

    #[test]
    fn status_bar_polls_and_clock_only_when_enabled() {
        let (mut app, mut rxs) = idle_app();
        assert!(app.deadlines(Instant::now()).is_empty());
        app.config.ui.status_bar.enabled = true;
        app.on_tick();
        let cmds = drain(&mut rxs[0]);
        assert!(methods(&cmds).contains(&"status.segments".to_string()));
        let now = Instant::now();
        let d = app.deadlines(now);
        // In flight: only the clock (default segments include `clock`).
        assert_eq!(d.names(), vec!["statusbar.clock"]);
        let clock = d.get("statusbar.clock").unwrap();
        assert!(clock > now && clock <= now + Duration::from_secs(60));
        answer(&mut app, 0, cmds[0].0, json!({"segments": {}}));
        let req = app.parity.status.last_req.unwrap();
        let d = app.deadlines(Instant::now());
        assert_eq!(d.get("statusbar"), Some(req + Duration::from_secs(10)));
        app.parity.status.stale = true;
        let d = app.deadlines(Instant::now());
        assert_eq!(d.get("statusbar"), Some(req + Duration::from_secs(1)));
        // Without a clock segment the minute does not wake the loop.
        for side in [
            &mut app.config.ui.status_bar.left,
            &mut app.config.ui.status_bar.center,
            &mut app.config.ui.status_bar.right,
        ] {
            side.retain(|s| s != "clock");
        }
        assert_eq!(app.deadlines(Instant::now()).names(), vec!["statusbar"]);
    }

    #[test]
    fn mirror_poll_arms_only_with_mirrors_or_remote_previews() {
        let (mut app, mut rxs) = idle_app();
        assert!(
            app.deadlines(Instant::now())
                .get("browser.mirrors")
                .is_none()
        );
        app.browser.mirrors.push(crate::browser::MirrorInfo {
            machine: "devbox".into(),
            preview: "pv1".into(),
            handle: "1".into(),
            port: 4000,
        });
        let now = Instant::now();
        assert!(
            app.deadlines(now)
                .get("browser.mirrors")
                .is_some_and(|t| t <= now)
        );
        app.on_tick();
        assert!(methods(&drain(&mut rxs[0])).contains(&"preview.status".to_string()));
        let t = Instant::now();
        let at = app.deadlines(t).get("browser.mirrors").unwrap();
        assert!(at > t + Duration::from_secs(9) && at <= t + Duration::from_secs(10));
    }

    #[test]
    fn scroll_report_arms_the_coalescing_interval() {
        let (mut app, _rxs) = idle_app();
        app.machines[0]
            .features
            .push(crate::plugins::SCROLL_FEATURE.to_string());
        assert!(app.deadlines(Instant::now()).is_empty());
        let now = Instant::now();
        crate::plugins::test_pending_scroll(&mut app, now);
        let d = app.deadlines(now);
        assert_eq!(d.names(), vec!["plugins.scroll"]);
        assert_eq!(
            d.get("plugins.scroll"),
            Some(now + Duration::from_millis(150))
        );
    }

    #[test]
    fn wake_at_adds_slack_and_never_spins() {
        let now = Instant::now();
        assert_eq!(wake_at(None, now), None);
        assert_eq!(
            wake_at(Some(now + Duration::from_secs(2)), now),
            Some(now + Duration::from_secs(2) + SLACK)
        );
        assert_eq!(
            wake_at(Some(now - Duration::from_secs(1)), now),
            Some(now + MIN_NAP + SLACK)
        );
    }
}
