//! Machine-wide port block leases (05 §6).
//!
//! State lives in a directory (`<state>/machine` in production): `ports.lock`
//! is `flock(2)`ed exclusively around every read-modify-write of
//! `ports.json`, so any number of threads, sessions and processes share one
//! lease table. (Deviation: JSON + flock instead of SQLite; see crate docs.)

use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::{Ipv4Addr, TcpListener};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The pool of ports and the lease block size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortPool {
    pub start: u16,
    /// Inclusive.
    pub end: u16,
    pub block: u16,
}

impl Default for PortPool {
    fn default() -> Self {
        Self {
            start: 20000,
            end: 29999,
            block: 10,
        }
    }
}

impl PortPool {
    /// Parse `tasks.port_pool` (`"20000-29999"`) plus `tasks.port_block`.
    pub fn parse(range: &str, block: u16) -> Result<Self> {
        let bad = || Error::Config(format!("invalid port_pool {range:?}"));
        let (a, b) = range.split_once('-').ok_or_else(bad)?;
        let start: u16 = a.trim().parse().map_err(|_| bad())?;
        let end: u16 = b.trim().parse().map_err(|_| bad())?;
        if block == 0 || start == 0 || end < start {
            return Err(bad());
        }
        Ok(Self { start, end, block })
    }

    /// Block starts aligned to the block size that fit entirely in the pool.
    fn candidates(&self) -> impl Iterator<Item = u16> + use<> {
        let block = self.block as u32;
        let first = (self.start as u32).div_ceil(block) * block;
        let (end, b) = (self.end as u32, block);
        (first..=end)
            .step_by(block as usize)
            .filter(move |s| s + b - 1 <= end)
            .map(|s| s as u16)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub start: u16,
    /// Inclusive.
    pub end: u16,
    pub task_id: String,
    pub session: String,
    /// Pid that owns the lease; the lease expires once it is dead.
    /// `None` = never expires (e.g. parked tasks).
    pub owner_pid: Option<u32>,
    /// Unix seconds.
    pub created_at: u64,
}

impl Lease {
    pub fn count(&self) -> u16 {
        self.end - self.start + 1
    }

    /// Port `base + offset`, if inside the block.
    pub fn port(&self, offset: u16) -> Option<u16> {
        (offset < self.count()).then(|| self.start + offset)
    }
}

#[derive(Debug, Clone)]
pub struct LeaseRequest {
    pub task_id: String,
    pub session: String,
    pub owner_pid: Option<u32>,
}

impl LeaseRequest {
    /// Owned by the current process.
    pub fn new(task_id: impl Into<String>, session: impl Into<String>) -> Self {
        Self {
            task_id: task_id.into(),
            session: session.into(),
            owner_pid: Some(std::process::id()),
        }
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct LeaseFile {
    #[serde(default)]
    leases: Vec<Lease>,
}

/// State of the pool for `vibeke doctor` (05 §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolHealth {
    /// Aligned blocks that fit in the pool.
    pub capacity: u32,
    /// Blocks not overlapped by any live lease.
    pub free: u32,
    pub leased: u32,
    pub ephemeral: Option<(u16, u16)>,
    /// Human-readable problems: exhaustion, ephemeral overlap, leases that
    /// fall outside the pool or overlap each other.
    pub warnings: Vec<String>,
}

/// The OS's ephemeral (outgoing connection) port range, if it can be read:
/// `/proc/sys/net/ipv4/ip_local_port_range` on Linux, `sysctl` on macOS.
pub fn ephemeral_range() -> Option<(u16, u16)> {
    #[cfg(target_os = "linux")]
    {
        let t = fs::read_to_string("/proc/sys/net/ipv4/ip_local_port_range").ok()?;
        let mut it = t.split_whitespace().filter_map(|x| x.parse().ok());
        Some((it.next()?, it.next()?))
    }
    #[cfg(target_os = "macos")]
    {
        let get = |k: &str| -> Option<u16> {
            let o = std::process::Command::new("sysctl")
                .args(["-n", k])
                .output()
                .ok()?;
            String::from_utf8_lossy(&o.stdout).trim().parse().ok()
        };
        Some((
            get("net.inet.ip.portrange.first")?,
            get("net.inet.ip.portrange.last")?,
        ))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        None
    }
}

/// Pure health computation: see [`PoolHealth`]. A pool is *exhausted* when no
/// free block is left and *low* when at most one tenth remains.
pub fn pool_health(pool: PortPool, leases: &[Lease], ephemeral: Option<(u16, u16)>) -> PoolHealth {
    let blocks: Vec<u16> = pool.candidates().collect();
    let capacity = blocks.len() as u32;
    let b = pool.block;
    let free = blocks
        .iter()
        .filter(|&&s| !leases.iter().any(|l| l.start < s + b && s <= l.end))
        .count() as u32;
    let mut warnings = Vec::new();
    if let Some((lo, hi)) = ephemeral
        && pool.start <= hi
        && lo <= pool.end
    {
        warnings.push(format!(
            "tasks.port_pool {}-{} overlaps the OS ephemeral port range {lo}-{hi}: the kernel may hand pool ports to outgoing connections",
            pool.start, pool.end
        ));
    }
    if capacity == 0 {
        warnings.push(format!(
            "tasks.port_pool {}-{} holds no aligned block of {} ports",
            pool.start, pool.end, pool.block
        ));
    } else if free == 0 {
        warnings.push(format!(
            "port range exhausted: all {capacity} blocks of {} ports are leased; new tasks will start without ports (finish tasks or widen tasks.port_pool)",
            pool.block
        ));
    } else if free * 10 <= capacity {
        warnings.push(format!(
            "port range nearly exhausted: {free} of {capacity} blocks free"
        ));
    }
    for l in leases {
        if l.start < pool.start || l.end > pool.end {
            warnings.push(format!(
                "lease {}-{} of task {} lies outside tasks.port_pool {}-{} (the pool was changed)",
                l.start, l.end, l.task_id, pool.start, pool.end
            ));
        }
    }
    for (i, a) in leases.iter().enumerate() {
        for c in &leases[i + 1..] {
            if a.start <= c.end && c.start <= a.end {
                warnings.push(format!(
                    "leases {}-{} (task {}) and {}-{} (task {}) overlap",
                    a.start, a.end, a.task_id, c.start, c.end, c.task_id
                ));
            }
        }
    }
    PoolHealth {
        capacity,
        free,
        leased: leases.len() as u32,
        ephemeral,
        warnings,
    }
}

/// Handle on the machine-wide lease table in `dir`.
#[derive(Debug, Clone)]
pub struct PortLeases {
    dir: PathBuf,
    pool: PortPool,
    probe: bool,
}

pub(crate) fn pid_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn is_live(l: &Lease) -> bool {
    l.owner_pid.is_none_or(pid_alive)
}

fn port_free(port: u16) -> bool {
    TcpListener::bind((Ipv4Addr::LOCALHOST, port)).is_ok()
}

struct LockGuard(File);

impl Drop for LockGuard {
    fn drop(&mut self) {
        // SAFETY: unlocking our own fd.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

impl PortLeases {
    pub fn new(state_dir: impl Into<PathBuf>, pool: PortPool) -> Self {
        Self {
            dir: state_dir.into(),
            pool,
            probe: true,
        }
    }

    /// Disable the bind-test of candidate ports (tests only).
    pub fn with_probe(mut self, probe: bool) -> Self {
        self.probe = probe;
        self
    }

    pub fn pool(&self) -> PortPool {
        self.pool
    }

    fn lock(&self) -> Result<LockGuard> {
        fs::create_dir_all(&self.dir)?;
        let f = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.dir.join("ports.lock"))?;
        loop {
            // SAFETY: flock on a valid fd we own.
            let r = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) };
            if r == 0 {
                return Ok(LockGuard(f));
            }
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e.into());
            }
        }
    }

    fn file(&self) -> PathBuf {
        self.dir.join("ports.json")
    }

    fn load(&self) -> Result<LeaseFile> {
        match fs::read(self.file()) {
            Ok(b) if b.iter().all(u8::is_ascii_whitespace) => Ok(LeaseFile::default()),
            Ok(b) => Ok(serde_json::from_slice(&b)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LeaseFile::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn store(&self, f: &LeaseFile) -> Result<()> {
        let tmp = self
            .dir
            .join(format!("ports.json.tmp{}", std::process::id()));
        let mut t = File::create(&tmp)?;
        t.write_all(&serde_json::to_vec_pretty(f)?)?;
        t.sync_all()?;
        fs::rename(&tmp, self.file())?;
        Ok(())
    }

    /// Lease a block. Idempotent per `task_id`: an existing lease is returned
    /// (and re-owned by the caller). Ports in the block are bind-tested on
    /// 127.0.0.1 first; occupied blocks are skipped.
    pub fn lease(&self, req: &LeaseRequest) -> Result<Lease> {
        self.lease_sized(req, self.pool.block)
    }

    /// Like [`lease`](Self::lease) with a block size of `block` ports
    /// (`[ports] count` of the repo's task file), aligned to that size.
    pub fn lease_sized(&self, req: &LeaseRequest, block: u16) -> Result<Lease> {
        let pool = PortPool {
            block: block.clamp(1, self.pool.end - self.pool.start + 1),
            ..self.pool
        };
        let _g = self.lock()?;
        let mut f = self.load()?;
        f.leases.retain(is_live);
        if let Some(l) = f.leases.iter_mut().find(|l| l.task_id == req.task_id) {
            l.owner_pid = req.owner_pid;
            l.session = req.session.clone();
            let l = l.clone();
            self.store(&f)?;
            return Ok(l);
        }
        let b = pool.block;
        for s in pool.candidates() {
            let e = s + b - 1;
            if f.leases.iter().any(|l| l.start <= e && s <= l.end) {
                continue;
            }
            if self.probe && !(s..=e).all(port_free) {
                continue;
            }
            let l = Lease {
                start: s,
                end: e,
                task_id: req.task_id.clone(),
                session: req.session.clone(),
                owner_pid: req.owner_pid,
                created_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
            };
            f.leases.push(l.clone());
            self.store(&f)?;
            return Ok(l);
        }
        // Persist the expiry pruning even on failure.
        self.store(&f)?;
        Err(Error::PortsExhausted)
    }

    /// Move `task_id` to a different block of the same size (05 §6 `task ports --re-lease`,
    /// after a dev server hit `EADDRINUSE` in its block): the old block is never handed back,
    /// occupied blocks are skipped as usual, and the old lease is replaced in one locked
    /// update. Returns `(old, new)`; without an old lease this is a plain lease of
    /// `default_block` ports.
    pub fn re_lease(
        &self,
        req: &LeaseRequest,
        default_block: u16,
    ) -> Result<(Option<Lease>, Lease)> {
        let _g = self.lock()?;
        let mut f = self.load()?;
        f.leases.retain(is_live);
        let old = f.leases.iter().find(|l| l.task_id == req.task_id).cloned();
        let block = old.as_ref().map_or(default_block, Lease::count);
        let pool = PortPool {
            block: block.clamp(1, self.pool.end - self.pool.start + 1),
            ..self.pool
        };
        let b = pool.block;
        for s in pool.candidates() {
            let e = s + b - 1;
            if old.as_ref().is_some_and(|o| o.start <= e && s <= o.end) {
                continue;
            }
            if f.leases
                .iter()
                .any(|l| l.task_id != req.task_id && l.start <= e && s <= l.end)
            {
                continue;
            }
            if self.probe && !(s..=e).all(port_free) {
                continue;
            }
            let l = Lease {
                start: s,
                end: e,
                task_id: req.task_id.clone(),
                session: req.session.clone(),
                owner_pid: req.owner_pid,
                created_at: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs()),
            };
            f.leases.retain(|x| x.task_id != req.task_id);
            f.leases.push(l.clone());
            self.store(&f)?;
            return Ok((old, l));
        }
        self.store(&f)?;
        Err(Error::PortsExhausted)
    }

    /// Release the lease of `task_id`; returns whether one existed.
    pub fn release(&self, task_id: &str) -> Result<bool> {
        let _g = self.lock()?;
        let mut f = self.load()?;
        let before = f.leases.len();
        f.leases.retain(|l| l.task_id != task_id && is_live(l));
        let removed = f.leases.len() != before;
        self.store(&f)?;
        Ok(removed)
    }

    /// Change the owner of a lease (e.g. `None` when a task is parked).
    pub fn set_owner(&self, task_id: &str, owner_pid: Option<u32>) -> Result<bool> {
        let _g = self.lock()?;
        let mut f = self.load()?;
        let mut found = false;
        if let Some(l) = f.leases.iter_mut().find(|l| l.task_id == task_id) {
            l.owner_pid = owner_pid;
            found = true;
        }
        self.store(&f)?;
        Ok(found)
    }

    /// Live leases (expired ones are filtered out but not removed).
    pub fn list(&self) -> Result<Vec<Lease>> {
        let _g = self.lock()?;
        let mut v = self.load()?.leases;
        v.retain(is_live);
        v.sort_by_key(|l| l.start);
        Ok(v)
    }

    pub fn lease_for(&self, task_id: &str) -> Result<Option<Lease>> {
        Ok(self.list()?.into_iter().find(|l| l.task_id == task_id))
    }

    /// Drop expired leases; returns them.
    pub fn gc(&self) -> Result<Vec<Lease>> {
        let _g = self.lock()?;
        let mut f = self.load()?;
        let (live, dead): (Vec<_>, Vec<_>) = f.leases.drain(..).partition(is_live);
        f.leases = live;
        if !dead.is_empty() {
            self.store(&f)?;
        }
        Ok(dead)
    }

    /// Pool health for `vibeke doctor` (reads the live leases).
    pub fn health(&self) -> Result<PoolHealth> {
        Ok(pool_health(self.pool, &self.list()?, ephemeral_range()))
    }

    pub fn state_dir(&self) -> &Path {
        &self.dir
    }
}
