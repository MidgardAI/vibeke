//! Request scheduling (14 §8): a finite concurrency gate that prioritizes interactive work.
//!
//! - `capacity` requests run at once; the rest wait in arrival order within their class.
//! - When a slot frees, a waiting **interactive** request always goes before a waiting
//!   background one.
//! - Background work can never hold every slot: with capacity above one it may hold at most
//!   `capacity - 1`; with capacity one it takes the slot only when nothing interactive is
//!   waiting, and it is **never preempted** once running (the interactive request simply waits
//!   for it).
//! - Capacity changes apply immediately (no restart): raising it admits waiters, lowering it
//!   stops new admissions until enough running requests finish.
//! - A dropped (cancelled or timed-out) wait leaves the queue without leaking a slot.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Priority {
    Interactive,
    Background,
}

struct Waiter {
    id: u64,
    prio: Priority,
    tx: oneshot::Sender<()>,
}

#[derive(Default)]
struct Inner {
    cap: usize,
    running: usize,
    running_bg: usize,
    next: u64,
    waiters: VecDeque<Waiter>,
}

impl Inner {
    fn bg_cap(&self) -> usize {
        if self.cap >= 2 { self.cap - 1 } else { 1 }
    }

    fn dispatch(&mut self) {
        while self.running < self.cap {
            let pick = self
                .waiters
                .iter()
                .position(|w| w.prio == Priority::Interactive)
                .or_else(|| {
                    if self.running_bg < self.bg_cap() {
                        self.waiters
                            .iter()
                            .position(|w| w.prio == Priority::Background)
                    } else {
                        None
                    }
                });
            let Some(i) = pick else { break };
            let w = self.waiters.remove(i).expect("index is in range");
            self.running += 1;
            if w.prio == Priority::Background {
                self.running_bg += 1;
            }
            if w.tx.send(()).is_err() {
                // The wait was dropped between queueing and dispatch: give the slot back.
                self.release(w.prio);
            }
        }
    }

    fn release(&mut self, prio: Priority) {
        self.running = self.running.saturating_sub(1);
        if prio == Priority::Background {
            self.running_bg = self.running_bg.saturating_sub(1);
        }
    }
}

#[derive(Clone, Default)]
pub struct PriorityGate {
    inner: Arc<Mutex<Inner>>,
}

fn lk(m: &Mutex<Inner>) -> MutexGuard<'_, Inner> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A held slot; dropping it frees the slot and admits the next waiter.
pub struct Permit {
    gate: PriorityGate,
    prio: Priority,
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut g = lk(&self.gate.inner);
        g.release(self.prio);
        g.dispatch();
    }
}

/// A queued wait. If it is dropped before the permit is claimed it leaves the queue (or, if
/// it was already granted, returns the slot).
struct Pending {
    gate: PriorityGate,
    id: u64,
    prio: Priority,
    claimed: bool,
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.claimed {
            return;
        }
        let mut g = lk(&self.gate.inner);
        if let Some(i) = g.waiters.iter().position(|w| w.id == self.id) {
            g.waiters.remove(i);
        } else {
            // Granted but never claimed.
            g.release(self.prio);
            g.dispatch();
        }
    }
}

impl PriorityGate {
    pub fn new(capacity: usize) -> Self {
        let g = PriorityGate::default();
        g.set_capacity(capacity);
        g
    }

    /// Set the number of concurrent slots (at least one). Takes effect immediately.
    pub fn set_capacity(&self, capacity: usize) {
        let mut g = lk(&self.inner);
        g.cap = capacity.max(1);
        g.dispatch();
    }

    pub fn capacity(&self) -> usize {
        lk(&self.inner).cap
    }

    /// `(running, waiting)`.
    pub fn stats(&self) -> (usize, usize) {
        let g = lk(&self.inner);
        (g.running, g.waiters.len())
    }

    /// Wait for a slot. Cancel-safe: dropping the future leaves the queue.
    pub async fn acquire(&self, prio: Priority) -> Permit {
        let (tx, rx) = oneshot::channel();
        let id = {
            let mut g = lk(&self.inner);
            g.next += 1;
            let id = g.next;
            g.waiters.push_back(Waiter { id, prio, tx });
            g.dispatch();
            id
        };
        let mut pending = Pending {
            gate: self.clone(),
            id,
            prio,
            claimed: false,
        };
        // The sender is only dropped unsent when the gate itself is gone; treat that as
        // granted so a shutdown never wedges a waiter.
        let _ = rx.await;
        pending.claimed = true;
        Permit {
            gate: self.clone(),
            prio,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    async fn waits(g: &PriorityGate, p: Priority) -> bool {
        timeout(Duration::from_millis(80), g.acquire(p))
            .await
            .is_err()
    }

    #[tokio::test]
    async fn capacity_bounds_concurrency() {
        let g = PriorityGate::new(2);
        let a = g.acquire(Priority::Interactive).await;
        let _b = g.acquire(Priority::Interactive).await;
        assert!(waits(&g, Priority::Interactive).await, "the third waits");
        assert_eq!(g.stats(), (2, 0), "a dropped wait leaves the queue");
        drop(a);
        let _c = timeout(Duration::from_secs(1), g.acquire(Priority::Interactive))
            .await
            .expect("a freed slot admits the next request");
        assert_eq!(g.stats().0, 2);
    }

    #[tokio::test]
    async fn interactive_goes_before_queued_background() {
        let g = PriorityGate::new(1);
        let held = g.acquire(Priority::Interactive).await;
        let order = Arc::new(Mutex::new(vec![]));
        let mut tasks = vec![];
        for (name, prio) in [
            ("bg1", Priority::Background),
            ("bg2", Priority::Background),
            ("ui", Priority::Interactive),
        ] {
            let (g2, o2) = (g.clone(), order.clone());
            tasks.push(tokio::spawn(async move {
                let _p = g2.acquire(prio).await;
                o2.lock().unwrap().push(name);
                tokio::time::sleep(Duration::from_millis(20)).await;
            }));
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(g.stats(), (1, 3));
        drop(held);
        for t in tasks {
            t.await.unwrap();
        }
        assert_eq!(*order.lock().unwrap(), vec!["ui", "bg1", "bg2"]);
    }

    #[tokio::test]
    async fn background_cannot_take_every_slot() {
        let g = PriorityGate::new(2);
        let _bg = g.acquire(Priority::Background).await;
        assert!(
            waits(&g, Priority::Background).await,
            "a second background request waits although a slot is free"
        );
        let _ui = timeout(Duration::from_secs(1), g.acquire(Priority::Interactive))
            .await
            .expect("the spare slot is for interactive work");
        assert_eq!(g.stats(), (2, 0));
    }

    #[tokio::test]
    async fn with_one_slot_background_yields_priority_but_is_never_preempted() {
        let g = PriorityGate::new(1);
        let bg = g.acquire(Priority::Background).await;
        // An interactive request waits for the running background one (no preemption).
        let g2 = g.clone();
        let ui = tokio::spawn(async move {
            let _p = g2.acquire(Priority::Interactive).await;
        });
        let g3 = g.clone();
        let bg2 = tokio::spawn(async move {
            let _p = g3.acquire(Priority::Background).await;
        });
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(!ui.is_finished() && !bg2.is_finished());
        assert_eq!(g.stats(), (1, 2));
        drop(bg);
        timeout(Duration::from_secs(2), ui).await.unwrap().unwrap();
        timeout(Duration::from_secs(2), bg2).await.unwrap().unwrap();
        assert_eq!(g.stats(), (0, 0));
    }

    #[tokio::test]
    async fn capacity_changes_apply_without_a_restart() {
        let g = PriorityGate::new(1);
        let _a = g.acquire(Priority::Interactive).await;
        let g2 = g.clone();
        let b = tokio::spawn(async move {
            let _p = g2.acquire(Priority::Interactive).await;
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!b.is_finished());
        g.set_capacity(2);
        timeout(Duration::from_secs(1), b).await.unwrap().unwrap();
        assert_eq!(g.capacity(), 2);
        g.set_capacity(0);
        assert_eq!(g.capacity(), 1, "never below one");
    }

    #[tokio::test]
    async fn a_cancelled_wait_never_leaks_a_slot() {
        let g = PriorityGate::new(1);
        let held = g.acquire(Priority::Interactive).await;
        for _ in 0..5 {
            assert!(waits(&g, Priority::Interactive).await);
        }
        assert_eq!(g.stats(), (1, 0));
        drop(held);
        assert_eq!(g.stats(), (0, 0));
        let _p = timeout(Duration::from_secs(1), g.acquire(Priority::Interactive))
            .await
            .unwrap();
    }
}
