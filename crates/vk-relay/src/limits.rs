//! Limits and token buckets (spec 16 §6.4).

use std::collections::HashMap;
use std::hash::Hash;
use std::net::IpAddr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Limits {
    pub max_message: usize,
    pub ip_new_per_min: u32,
    pub ip_concurrent: usize,
    pub auth_timeout: Duration,
    pub accept_timeout: Duration,
    pub host_pending: usize,
    pub host_spliced: usize,
    pub host_announces_per_min: u32,
    pub conn_bytes_per_sec: u64,
    pub conn_burst_bytes: u64,
    pub conn_msgs_per_sec: u32,
    pub idle_timeout: Duration,
    pub write_timeout: Duration,
    pub max_hosts: usize,
    pub max_conns: usize,
    /// `/v1/connect` announces one IP may cause for one host per minute.
    pub ip_host_announces_per_min: u32,
    /// Control (`/v1/host`) and accept sockets per IP that have not authenticated yet.
    pub ip_unauthenticated: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_message: 128 * 1024,
            ip_new_per_min: 30,
            ip_concurrent: 64,
            auth_timeout: Duration::from_secs(10),
            accept_timeout: Duration::from_secs(10),
            host_pending: 8,
            host_spliced: 32,
            host_announces_per_min: 60,
            conn_bytes_per_sec: 1024 * 1024,
            conn_burst_bytes: 4 * 1024 * 1024,
            conn_msgs_per_sec: 200,
            idle_timeout: Duration::from_secs(120),
            write_timeout: Duration::from_secs(30),
            max_hosts: 10_000,
            max_conns: 50_000,
            ip_host_announces_per_min: 10,
            ip_unauthenticated: 8,
        }
    }
}

/// A classic token bucket.
#[derive(Debug)]
pub struct Bucket {
    capacity: f64,
    per_sec: f64,
    tokens: f64,
    last: Instant,
}

impl Bucket {
    pub fn new(capacity: f64, per_sec: f64) -> Self {
        Bucket {
            capacity,
            per_sec,
            tokens: capacity,
            last: Instant::now(),
        }
    }
    pub fn per_minute(n: u32) -> Self {
        Bucket::new(n as f64, n as f64 / 60.0)
    }
    fn refill(&mut self) {
        let now = Instant::now();
        self.tokens = (self.tokens + now.duration_since(self.last).as_secs_f64() * self.per_sec)
            .min(self.capacity);
        self.last = now;
    }
    pub fn take(&mut self, n: f64) -> bool {
        self.refill();
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
    /// Take `n` (may go into debt) and return how long to wait before the debt is repaid.
    pub fn take_wait(&mut self, n: f64) -> Duration {
        self.refill();
        self.tokens -= n;
        if self.tokens >= 0.0 {
            Duration::ZERO
        } else {
            Duration::from_secs_f64(-self.tokens / self.per_sec)
        }
    }
    fn full(&mut self) -> bool {
        self.refill();
        self.tokens >= self.capacity
    }
}

/// Per-key (per-IP, or per IP and host) buckets with opportunistic cleanup.
pub struct RateMap<K = IpAddr> {
    per_min: u32,
    map: Mutex<HashMap<K, Bucket>>,
}

impl<K: Hash + Eq> RateMap<K> {
    pub fn new(per_min: u32) -> Self {
        RateMap {
            per_min,
            map: Mutex::new(HashMap::new()),
        }
    }
    pub fn allow(&self, key: K) -> bool {
        let mut m = self.map.lock().unwrap();
        if m.len() > 50_000 {
            m.retain(|_, b| !b.full());
        }
        m.entry(key)
            .or_insert_with(|| Bucket::per_minute(self.per_min))
            .take(1.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_limits() {
        let mut b = Bucket::per_minute(2);
        assert!(b.take(1.0));
        assert!(b.take(1.0));
        assert!(!b.take(1.0));
        let mut b = Bucket::new(10.0, 10.0);
        assert_eq!(b.take_wait(10.0), Duration::ZERO);
        assert!(b.take_wait(5.0) >= Duration::from_millis(400));
    }

    #[test]
    fn keyed_rate_is_per_key() {
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        let m: RateMap<(IpAddr, String)> = RateMap::new(2);
        assert!(m.allow((ip, "a".into())));
        assert!(m.allow((ip, "a".into())));
        assert!(!m.allow((ip, "a".into())));
        assert!(m.allow((ip, "b".into())));
    }
}
