//! Event fan-out (spec 16 §7.5): one server subscription, a ring of recent events, and per-device
//! replay by sequence number with an explicit reset when the gap cannot be filled.

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::broadcast;

pub const RING: usize = 5000;

#[derive(Debug, Clone)]
pub enum Fanout {
    Event(Value),
    Reset,
}

struct Ring {
    events: VecDeque<Value>,
    /// Every event with seq > floor is in `events` (None = unknown; replays must reset).
    floor: Option<u64>,
    /// Last server cursor seen (for resubscribing).
    cursor: Option<Value>,
}

pub struct Hub {
    ring: Mutex<Ring>,
    tx: broadcast::Sender<Fanout>,
    /// `gateway.request` events (transient, addressed to this gateway): never in the ring and
    /// never shown to devices.
    requests: broadcast::Sender<Value>,
}

pub fn seq_of(ev: &Value) -> u64 {
    ev.get("seq").and_then(|s| s.as_u64()).unwrap_or(0)
}

impl Default for Hub {
    fn default() -> Self {
        Hub {
            ring: Mutex::new(Ring {
                events: VecDeque::new(),
                floor: None,
                cursor: None,
            }),
            tx: broadcast::channel(1000).0,
            requests: broadcast::channel(64).0,
        }
    }
}

impl Hub {
    /// Server confirmed a subscription at cursor `at`.
    pub fn subscribed(&self, at: Value) {
        let mut r = self.ring.lock().unwrap();
        if r.floor.is_none() {
            r.floor = at
                .get("seq")
                .and_then(|s| s.as_u64())
                .or_else(|| at.as_u64());
        }
        if r.cursor.is_none() {
            r.cursor = Some(at);
        }
    }

    /// The full server cursor (machine, session, epoch) advanced to the last event we hold, so a
    /// replaced log or epoch is detected by the server instead of silently skipping events.
    pub fn resume_cursor(&self) -> Option<Value> {
        let r = self.ring.lock().unwrap();
        let last = r.events.back().map(seq_of);
        match (&r.cursor, last) {
            (Some(c), Some(seq)) if c.is_object() => {
                let mut c = c.clone();
                c["seq"] = seq.into();
                Some(c)
            }
            (Some(c), None) => Some(c.clone()),
            (_, Some(seq)) => Some(Value::from(seq)),
            (None, None) => None,
        }
    }

    /// Plain sequence number devices resume from.
    pub fn last_seq(&self) -> Option<u64> {
        let r = self.ring.lock().unwrap();
        r.events.back().map(seq_of).or_else(|| {
            r.cursor
                .as_ref()
                .and_then(|c| c.get("seq").and_then(|s| s.as_u64()).or_else(|| c.as_u64()))
        })
    }

    pub fn push(&self, ev: Value) {
        {
            let mut r = self.ring.lock().unwrap();
            let seq = seq_of(&ev);
            if r.events.back().is_some_and(|b| seq_of(b) >= seq) {
                return; // duplicate after a resubscribe
            }
            r.events.push_back(ev.clone());
            while r.events.len() > RING {
                let old = r.events.pop_front().map(|e| seq_of(&e));
                r.floor = old.max(r.floor);
            }
        }
        let _ = self.tx.send(Fanout::Event(ev));
    }

    /// Cursor no longer valid server-side: forget everything and tell devices to refetch.
    pub fn reset(&self) {
        let mut r = self.ring.lock().unwrap();
        r.events.clear();
        r.floor = None;
        r.cursor = None;
        drop(r);
        let _ = self.tx.send(Fanout::Reset);
    }

    /// A `gateway.request` event's data (`{id, method, params}`) for [`crate::bridge`].
    pub fn push_request(&self, data: Value) {
        let _ = self.requests.send(data);
    }

    pub fn subscribe_requests(&self) -> broadcast::Receiver<Value> {
        self.requests.subscribe()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Fanout> {
        self.tx.subscribe()
    }

    /// Events after `after`, or `None` when the ring cannot prove completeness.
    pub fn replay(&self, after: u64) -> Option<Vec<Value>> {
        let r = self.ring.lock().unwrap();
        let floor = r.floor?;
        if after < floor {
            return None;
        }
        Some(
            r.events
                .iter()
                .filter(|e| seq_of(e) > after)
                .cloned()
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replay_and_reset() {
        let h = Hub::default();
        assert!(h.replay(0).is_none());
        h.subscribed(json!({"seq": 10}));
        for s in 11..=13 {
            h.push(json!({"seq": s}));
        }
        h.push(json!({"seq": 12})); // duplicate ignored
        assert_eq!(h.replay(11).unwrap().len(), 2);
        assert_eq!(h.replay(10).unwrap().len(), 3);
        assert!(h.replay(9).is_none());
        h.reset();
        assert!(h.replay(13).is_none());
    }

    #[test]
    fn eviction_raises_floor() {
        let h = Hub::default();
        h.subscribed(json!({"seq": 0}));
        for s in 1..=(RING as u64 + 10) {
            h.push(json!({"seq": s}));
        }
        assert!(h.replay(5).is_none());
        assert!(h.replay(10).is_some());
    }
}
