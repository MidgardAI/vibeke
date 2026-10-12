//! Automation hold (spec 19 §4.2). While Vibeke types a prompt into an agent pane (a paste, a
//! short pause, then Enter), keystrokes, pastes and mouse input that people send to that pane
//! wait in a buffer. They reach the pane right after the Enter, in order, so a draft a person
//! starts in that gap is not submitted together with the prompt.
//!
//! A hold is short: it ends when its guard drops, and in any case after [`MAX_HOLD`]. Input
//! that arrives after the deadline flushes the buffer first and then goes through.

use crate::pane::{PaneCmd, PaneRt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The longest a hold keeps people's input back.
pub const MAX_HOLD: Duration = Duration::from_secs(2);

type Map = Arc<Mutex<HashMap<String, Hold>>>;

struct Hold {
    generation: u64,
    since: Instant,
    queue: Vec<(Arc<PaneRt>, PaneCmd)>,
}

/// The holds of all panes (`Server::input_hold`).
#[derive(Default)]
pub struct Holds {
    inner: Map,
    next: AtomicU64,
}

/// Ends its hold when dropped and sends the buffered input to the pane. Guards taken while the
/// pane is already held share that hold: the first one dropped ends it.
#[must_use = "the hold ends when the guard is dropped"]
pub struct HoldGuard {
    inner: Map,
    pane: String,
    generation: u64,
}

impl Drop for HoldGuard {
    fn drop(&mut self) {
        release(&self.inner, &self.pane, self.generation);
    }
}

impl Holds {
    /// Hold people's input for `pane` until the guard drops (at most [`MAX_HOLD`]).
    pub fn hold(&self, pane: &str) -> HoldGuard {
        let mut m = self.inner.lock().unwrap();
        let live = m
            .get(pane)
            .filter(|h| h.since.elapsed() < MAX_HOLD)
            .map(|h| h.generation);
        let generation = match live {
            Some(g) => g,
            None => {
                if let Some(old) = m.remove(pane) {
                    flush(old.queue);
                }
                let g = self.next.fetch_add(1, Ordering::Relaxed) + 1;
                m.insert(
                    pane.to_string(),
                    Hold {
                        generation: g,
                        since: Instant::now(),
                        queue: Vec::new(),
                    },
                );
                // The deadline also passes when no further input arrives to notice it.
                if let Ok(rt) = tokio::runtime::Handle::try_current() {
                    let inner = self.inner.clone();
                    let pane = pane.to_string();
                    rt.spawn(async move {
                        tokio::time::sleep(MAX_HOLD).await;
                        release(&inner, &pane, g);
                    });
                }
                g
            }
        };
        HoldGuard {
            inner: self.inner.clone(),
            pane: pane.to_string(),
            generation,
        }
    }

    /// Send `cmd` (a person's input) to `rt` now, or buffer it while `pane` is held. Returns
    /// whether it was buffered.
    pub fn send(&self, pane: &str, rt: &Arc<PaneRt>, cmd: PaneCmd) -> bool {
        let mut m = self.inner.lock().unwrap();
        if let Some(h) = m.get_mut(pane) {
            if h.since.elapsed() < MAX_HOLD {
                h.queue.push((rt.clone(), cmd));
                return true;
            }
            // An expired hold: what waited goes first.
            if let Some(old) = m.remove(pane) {
                flush(old.queue);
            }
        }
        rt.send(cmd);
        false
    }

    /// Whether `pane` is held right now.
    pub fn is_held(&self, pane: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .get(pane)
            .is_some_and(|h| h.since.elapsed() < MAX_HOLD)
    }
}

/// End hold `generation` of `pane` (a later hold is left alone) and send what it buffered.
fn release(inner: &Map, pane: &str, generation: u64) {
    let mut m = inner.lock().unwrap();
    if m.get(pane).is_some_and(|h| h.generation == generation)
        && let Some(h) = m.remove(pane)
    {
        // Under the lock, so input that arrives meanwhile cannot overtake the buffer.
        flush(h.queue);
    }
}

fn flush(queue: Vec<(Arc<PaneRt>, PaneCmd)>) {
    for (rt, cmd) in queue {
        rt.send(cmd);
    }
}

#[cfg(test)]
#[path = "input_hold_tests.rs"]
mod tests;
