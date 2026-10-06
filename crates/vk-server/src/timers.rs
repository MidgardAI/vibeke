//! Coalesced server timers (spec 10 §1.3: an idle server wakes at most 2×/s, no polling loops).
//!
//! Pane snapshot deadlines live in one server-wide [`Scheduler`] instead of a ticking interval
//! per pane: a pane arms a deadline only while it has pending work (output not yet covered by a
//! snapshot), deadlines are rounded up to a [`GRID`] so panes that went quiet together share
//! one wakeup, and a fully idle pane has nothing armed at all. A single task sleeps until the
//! earliest deadline (or forever) and delivers [`PaneCmd::SnapshotDue`] to the pane, whose loop
//! decides whether to snapshot now or re-arm for later.
//!
//! [`Activity`] + [`Backoff`] pace pollers that cannot be fully event-driven (preview listener
//! discovery): fast while panes are active, doubling to a long period when nothing happens,
//! and woken at once by the next output.
//!
//! The rest of the idle path is event-driven elsewhere: housekeeping runs only when archive
//! rows or a storage failure wake it (`run.rs`), the sandbox tick stops when no context or
//! broker is left, and the review live-state watcher blocks on the model revision.

use crate::pane::PaneCmd;
use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::{Notify, mpsc};
use tokio::time::Instant;

/// Deadlines are rounded up to this grid so nearby deadlines fire in one wakeup.
pub const GRID: Duration = Duration::from_millis(250);

#[derive(Default)]
struct Inner {
    /// Grid origin (set on first use).
    base: Option<Instant>,
    due: BTreeSet<(Instant, String)>,
    by_key: HashMap<String, (Instant, mpsc::UnboundedSender<PaneCmd>)>,
}

pub struct Scheduler {
    inner: Mutex<Inner>,
    wake: Arc<Notify>,
    started: AtomicBool,
    arms: AtomicU64,
    fires: AtomicU64,
    wakeups: AtomicU64,
}

impl Default for Scheduler {
    fn default() -> Self {
        Scheduler {
            inner: Mutex::default(),
            wake: Arc::new(Notify::new()),
            started: AtomicBool::new(false),
            arms: AtomicU64::new(0),
            fires: AtomicU64::new(0),
            wakeups: AtomicU64::new(0),
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        // Let the timer task notice the scheduler is gone and end.
        self.wake.notify_one();
    }
}

fn round_up(base: Instant, at: Instant) -> Instant {
    let off = at.saturating_duration_since(base).as_nanos();
    let g = GRID.as_nanos();
    let n = off.div_ceil(g);
    base + Duration::from_nanos((n * g) as u64)
}

impl Scheduler {
    /// Arm (or pull earlier) the deadline for `key`; at `at` (rounded up to [`GRID`]) the
    /// scheduler sends [`PaneCmd::SnapshotDue`] on `tx`. One entry per key: a later deadline
    /// than the one already armed is a no-op. Returns whether anything changed.
    pub fn arm(
        self: &Arc<Self>,
        key: &str,
        at: Instant,
        tx: &mpsc::UnboundedSender<PaneCmd>,
    ) -> bool {
        let earliest_changed = {
            let mut i = self.inner.lock().unwrap();
            let base = *i.base.get_or_insert_with(Instant::now);
            let at = round_up(base, at);
            if let Some((old, _)) = i.by_key.get(key) {
                if *old <= at {
                    return false;
                }
                let old = *old;
                i.due.remove(&(old, key.to_string()));
            }
            let prev_first = i.due.first().map(|(t, _)| *t);
            i.due.insert((at, key.to_string()));
            i.by_key.insert(key.to_string(), (at, tx.clone()));
            prev_first.is_none_or(|f| at < f)
        };
        self.arms.fetch_add(1, Ordering::Relaxed);
        self.ensure_task();
        if earliest_changed {
            self.wake.notify_one();
        }
        true
    }

    /// Drop the deadline for `key`, if any.
    pub fn cancel(&self, key: &str) {
        let was_first = {
            let mut i = self.inner.lock().unwrap();
            let Some((at, _)) = i.by_key.remove(key) else {
                return;
            };
            let first = i.due.first().is_some_and(|(t, k)| *t == at && k == key);
            i.due.remove(&(at, key.to_string()));
            first
        };
        if was_first {
            // Let the task drop (or move) its timer instead of waking for nothing.
            self.wake.notify_one();
        }
    }

    /// Deadlines currently armed (test hook / `server.status`).
    pub fn pending(&self) -> usize {
        self.inner.lock().unwrap().by_key.len()
    }

    /// Whether `key` has a deadline armed.
    pub fn is_armed(&self, key: &str) -> bool {
        self.inner.lock().unwrap().by_key.contains_key(key)
    }

    /// Deadlines armed since start (test hook: an idle session must not keep arming).
    pub fn arms(&self) -> u64 {
        self.arms.load(Ordering::Relaxed)
    }

    /// Deadlines delivered since start.
    pub fn fires(&self) -> u64 {
        self.fires.load(Ordering::Relaxed)
    }

    /// Times the timer task woke for a deadline.
    pub fn wakeups(&self) -> u64 {
        self.wakeups.load(Ordering::Relaxed)
    }

    fn next(&self) -> Option<Instant> {
        self.inner.lock().unwrap().due.first().map(|(t, _)| *t)
    }

    /// Remove and return every entry due at `now`.
    fn take_due(&self, now: Instant) -> Vec<(String, mpsc::UnboundedSender<PaneCmd>)> {
        let mut i = self.inner.lock().unwrap();
        let mut out = Vec::new();
        while let Some((t, _)) = i.due.first() {
            if *t > now {
                break;
            }
            let (_, key) = i.due.pop_first().expect("non-empty");
            if let Some((_, tx)) = i.by_key.remove(&key) {
                out.push((key, tx));
            }
        }
        out
    }

    fn ensure_task(self: &Arc<Self>) {
        if self.started.load(Ordering::Relaxed) {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }
        handle.spawn(run(Arc::downgrade(self), self.wake.clone()));
    }
}

async fn run(weak: Weak<Scheduler>, wake: Arc<Notify>) {
    loop {
        let next = match weak.upgrade() {
            Some(s) => s.next(),
            None => return,
        };
        match next {
            Some(at) => {
                tokio::select! {
                    _ = tokio::time::sleep_until(at) => {}
                    _ = wake.notified() => continue,
                }
            }
            None => {
                // Nothing armed: no timer at all until a pane arms one.
                wake.notified().await;
                continue;
            }
        }
        let Some(s) = weak.upgrade() else { return };
        s.wakeups.fetch_add(1, Ordering::Relaxed);
        for (_, tx) in s.take_due(Instant::now()) {
            s.fires.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(PaneCmd::SnapshotDue);
        }
    }
}

/// Periodic-work counters for `server.status` (`timers`): what an idle server is (not) doing.
pub fn status_json(server: &crate::Server) -> serde_json::Value {
    let t = &server.timers;
    let d = &server.previews.pace;
    serde_json::json!({
        "snapshot_pending": t.pending(),
        "snapshot_arms": t.arms(),
        "snapshot_fires": t.fires(),
        "scheduler_wakeups": t.wakeups(),
        "housekeeping_runs": server.housekeeping_runs.load(Ordering::Relaxed),
        "discovery_passes": d.passes(),
        "discovery_interval_ms": d.interval().as_millis() as u64,
        "sandbox_ticking": crate::sandbox::ticking(server),
    })
}

// ---- activity-paced polling ---------------------------------------------------------------

/// Recent pane activity for a poller that backs off when nothing happens (preview discovery).
/// [`Activity::touch`] is on the pane feed path: one atomic store, plus one notify only when
/// the poller is parked in a long back-off sleep.
pub struct Activity {
    base: std::time::Instant,
    last_ms: AtomicU64,
    parked: AtomicBool,
    wake: Notify,
    passes: AtomicU64,
    interval_ms: AtomicU64,
}

impl Default for Activity {
    fn default() -> Self {
        Activity {
            base: std::time::Instant::now(),
            last_ms: AtomicU64::new(0),
            parked: AtomicBool::new(false),
            wake: Notify::new(),
            passes: AtomicU64::new(0),
            interval_ms: AtomicU64::new(0),
        }
    }
}

impl Activity {
    fn now_ms(&self) -> u64 {
        self.base.elapsed().as_millis() as u64
    }

    /// Something happened (pane output): resets the back-off and wakes a parked poller.
    pub fn touch(&self) {
        self.last_ms.store(self.now_ms(), Ordering::Relaxed);
        if self.parked.load(Ordering::Relaxed) && self.parked.swap(false, Ordering::AcqRel) {
            self.wake.notify_one();
        }
    }

    /// Record activity without waking the poller (it is already awake handling it).
    pub fn note(&self) {
        self.last_ms.store(self.now_ms(), Ordering::Relaxed);
    }

    pub fn since_touch(&self) -> Duration {
        Duration::from_millis(
            self.now_ms()
                .saturating_sub(self.last_ms.load(Ordering::Relaxed)),
        )
    }

    /// The poller is about to sleep longer than its fast period: let `touch` wake it.
    pub fn park(&self) {
        self.parked.store(true, Ordering::Release);
    }

    pub fn unpark(&self) {
        self.parked.store(false, Ordering::Release);
    }

    /// Resolves when a `touch` arrived while parked.
    pub async fn woken(&self) {
        self.wake.notified().await
    }

    pub fn pass_done(&self, next: Duration) {
        self.passes.fetch_add(1, Ordering::Relaxed);
        self.interval_ms
            .store(next.as_millis() as u64, Ordering::Relaxed);
    }

    /// Completed poll passes (test hook / `server.status`).
    pub fn passes(&self) -> u64 {
        self.passes.load(Ordering::Relaxed)
    }

    /// The poller's current period.
    pub fn interval(&self) -> Duration {
        Duration::from_millis(self.interval_ms.load(Ordering::Relaxed))
    }
}

/// Poll period with back-off: `fast` while busy or within `window` of the last activity,
/// then doubling up to `max` while nothing happens.
#[derive(Debug, Clone)]
pub struct Backoff {
    pub fast: Duration,
    pub window: Duration,
    pub max: Duration,
    cur: Duration,
}

impl Backoff {
    pub fn new(fast: Duration, window: Duration, max: Duration) -> Self {
        Backoff {
            fast,
            window,
            max,
            cur: fast,
        }
    }

    /// Back to the fast period (activity seen).
    pub fn reset(&mut self) -> Duration {
        self.cur = self.fast;
        self.cur
    }

    /// The period until the next pass, given the time since the last activity and whether
    /// there is ongoing work that needs the fast period regardless.
    pub fn next(&mut self, since_activity: Duration, busy: bool) -> Duration {
        if busy || since_activity < self.window {
            return self.reset();
        }
        self.cur = (self.cur * 2).min(self.max).max(self.fast);
        self.cur
    }

    pub fn current(&self) -> Duration {
        self.cur
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_fast_while_busy_or_recent_then_doubles_to_cap() {
        let s = Duration::from_secs;
        let mut b = Backoff::new(s(2), s(30), s(30));
        assert_eq!(b.next(s(1), false), s(2));
        assert_eq!(b.next(s(29), false), s(2));
        assert_eq!(b.next(s(31), true), s(2), "busy keeps the fast period");
        let seq: Vec<u64> = (0..6).map(|_| b.next(s(60), false).as_secs()).collect();
        assert_eq!(seq, [4, 8, 16, 30, 30, 30]);
        // Activity resets it.
        assert_eq!(b.next(s(0), false), s(2));
        assert_eq!(b.next(s(40), false), s(4));
        assert_eq!(b.reset(), s(2));
    }

    #[tokio::test]
    async fn activity_wakes_only_a_parked_poller() {
        let a = Arc::new(Activity::default());
        // Not parked: touch records activity but stores no wakeup.
        a.touch();
        assert!(a.since_touch() < Duration::from_secs(1));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), a.woken())
                .await
                .is_err()
        );
        // Parked: the first touch wakes it, and unparks.
        a.park();
        let a2 = a.clone();
        let t = tokio::spawn(async move { a2.woken().await });
        tokio::task::yield_now().await;
        a.touch();
        tokio::time::timeout(Duration::from_secs(5), t)
            .await
            .expect("woken")
            .unwrap();
        assert!(!a.parked.load(Ordering::Relaxed));
    }

    fn due(rx: &mut mpsc::UnboundedReceiver<PaneCmd>) -> usize {
        let mut n = 0;
        while let Ok(c) = rx.try_recv() {
            assert!(matches!(c, PaneCmd::SnapshotDue));
            n += 1;
        }
        n
    }

    #[test]
    fn grid_rounding() {
        let b = Instant::now();
        assert_eq!(round_up(b, b), b);
        assert_eq!(round_up(b, b + Duration::from_millis(1)), b + GRID);
        assert_eq!(round_up(b, b + GRID), b + GRID);
        assert_eq!(
            round_up(b, b + GRID * 3 + Duration::from_nanos(1)),
            b + GRID * 4
        );
        // Before the origin: fire at the origin (immediately).
        assert_eq!(round_up(b, b - Duration::from_secs(1)), b);
    }

    /// Wait (real time, generous for loaded hosts) until `f` holds.
    async fn until(what: &str, f: impl Fn() -> bool) {
        let t0 = std::time::Instant::now();
        while !f() {
            assert!(t0.elapsed() < Duration::from_secs(10), "timed out: {what}");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn one_entry_per_key_and_coalesced_fire() {
        let s = Arc::new(Scheduler::default());
        let (tx_a, mut rx_a) = mpsc::unbounded_channel();
        let (tx_b, mut rx_b) = mpsc::unbounded_channel();
        let now = Instant::now();
        // The first arm sets the grid origin (≈ now); deadlines round up to 250 ms slots.
        assert!(s.arm("a", now + Duration::from_millis(1700), &tx_a));
        // A later deadline for the same key changes nothing, nor does one in the same slot;
        // an earlier slot replaces it.
        assert!(!s.arm("a", now + Duration::from_secs(5), &tx_a));
        assert!(!s.arm("a", now + Duration::from_millis(1650), &tx_a));
        assert!(s.arm("a", now + Duration::from_millis(700), &tx_a));
        // Another pane going quiet 20 ms later shares a's slot (750 ms).
        assert!(s.arm("b", now + Duration::from_millis(720), &tx_b));
        assert_eq!(s.pending(), 2);
        assert!(s.is_armed("a") && s.is_armed("b"));
        assert_eq!(s.arms(), 3);
        // Timers never fire early.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!((due(&mut rx_a), due(&mut rx_b)), (0, 0));
        until("both fired", || s.fires() == 2).await;
        assert_eq!((due(&mut rx_a), due(&mut rx_b)), (1, 1));
        assert_eq!(s.pending(), 0);
        assert_eq!(s.wakeups(), 1, "both deadlines share one wakeup");
    }

    #[tokio::test]
    async fn nothing_armed_means_no_wakeups() {
        let s = Arc::new(Scheduler::default());
        let (tx, mut rx) = mpsc::unbounded_channel();
        s.arm("a", Instant::now() + Duration::from_millis(100), &tx);
        until("fired", || s.fires() == 1).await;
        assert_eq!(due(&mut rx), 1);
        let w = s.wakeups();
        // Nothing armed: the task never wakes.
        tokio::time::sleep(Duration::from_millis(1200)).await;
        assert_eq!(s.wakeups(), w);
        assert_eq!(due(&mut rx), 0);
        // Cancel removes a pending deadline before it fires, and its timer with it.
        s.arm("a", Instant::now() + Duration::from_millis(300), &tx);
        s.cancel("a");
        assert_eq!(s.pending(), 0);
        tokio::time::sleep(Duration::from_millis(900)).await;
        assert_eq!(due(&mut rx), 0);
        assert_eq!(s.wakeups(), w);
        assert_eq!(s.fires(), 1);
    }
}
