//! Durable pending client operations (15 §10.3): every task mutation is written to a small JSON
//! file under the client's state dir **before** it is dispatched, keyed by its idempotency key.
//! Each client instance owns one file (`client-pending-<client_id>.json`) and holds an advisory
//! lock on a sibling `.live` file while it runs; on start (and reconnect) a client adopts the
//! files of clients that no longer hold theirs (crashed), under a directory-wide lock. No client
//! rewrites or deletes another live client's entries.
//! After a crash, restart or link drop the client asks the owner (`task.operation.get`) what
//! happened; a mutation whose outcome is unknown is never resubmitted automatically — the user
//! gets an explicit Retry that warns the earlier request may have taken effect.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpStatus {
    /// Dispatched (or about to be); no response observed yet.
    #[default]
    InFlight,
    /// The owner has no receipt (or it expired): the outcome is unknown.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingOp {
    pub key: String,
    pub method: String,
    pub params: Value,
    /// Machine label (stable across client restarts).
    pub machine: String,
    pub created_at_ms: i64,
    #[serde(default)]
    pub status: OpStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl PendingOp {
    /// Short human description ("Track task “Fix login”").
    pub fn describe(&self) -> String {
        let title = self
            .params
            .get("title")
            .and_then(Value::as_str)
            .map(|t| format!(" “{}”", crate::draw::truncate(t, 40)))
            .unwrap_or_default();
        let what = match self.method.as_str() {
            "task.track" => "Track task",
            "task.intent.update" => "Save task details",
            "task.bind" => "Continue task",
            "task.message.prepare" => "Prepare message",
            "task.message.send" => "Send message",
            "task.review.accept" => "Mark reviewed",
            "task.check.run" => "Run check",
            "task.review.snapshot" => "Snapshot uncommitted work",
            "task.review.request_reviewer" => "Prepare reviewer prompt",
            "task.review.start_reviewer" => "Start reviewer",
            "task.review.note.classify" => "Classify review note",
            "task.dependency.add" => "Add dependency link",
            "task.dependency.remove" => "Remove dependency link",
            "task.set" => "Set task effort",
            "draft.send" => "Send draft",
            other => other,
        };
        format!("{what}{title}")
    }
}

/// Default directory: `$VIBEKE_STATE_DIR/<session>/` (same root as the server's per-session
/// state: `$XDG_STATE_HOME/vibeke` or `~/.local/state/vibeke`). Each client instance writes its
/// own `client-pending-<client_id>.json` there.
pub fn default_dir(session: &str) -> PathBuf {
    let root = if let Some(d) = std::env::var_os("VIBEKE_STATE_DIR") {
        PathBuf::from(d)
    } else if let Some(d) = std::env::var_os("XDG_STATE_HOME") {
        PathBuf::from(d).join("vibeke")
    } else {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".local/state/vibeke")
    };
    root.join(session)
}

const PREFIX: &str = "client-pending-";
/// Pre-per-client single file (always adopted: nothing holds it any more).
const LEGACY: &str = "client-pending.json";
/// Directory-wide advisory lock held while reading/adopting other clients' files.
const DIR_LOCK: &str = "client-pending.lock";

fn file_name(client: &str) -> String {
    format!("{PREFIX}{client}.json")
}

fn live_lock_name(client: &str) -> String {
    format!("{PREFIX}{client}.live")
}

/// Client ids become file names: keep them to a safe alphabet.
fn sanitize(client: &str) -> String {
    client
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[derive(Debug, Default)]
pub struct PendingStore {
    /// This client's own file. `None`: memory only (tests that don't care about persistence).
    pub path: Option<PathBuf>,
    pub ops: Vec<PendingOp>,
    /// Keys dispatched over the current connection (a response may still arrive).
    pub live: HashSet<String>,
    /// Keys with a `task.operation.get` outstanding.
    pub reconciling: HashSet<String>,
    /// Recent reconciliation results shown in the pending-operations popup.
    pub outcomes: Vec<String>,
    pub load_error: Option<String>,
    /// Number of operations adopted from crashed clients' files.
    pub adopted: usize,
    /// Held (exclusive `flock`) for this client's lifetime: tells other clients we're alive, so
    /// they never adopt or delete our entries. The kernel releases it when we crash.
    live_lock: Option<std::fs::File>,
    tmp_seq: std::cell::Cell<u64>,
}

impl PendingStore {
    /// Open this client's store in `dir` (one file per client instance) and adopt orphaned
    /// entries left by crashed clients of the same session.
    pub fn open(dir: PathBuf, client_id: &str) -> Self {
        let client = sanitize(client_id);
        let mut s = PendingStore::default();
        s.path = Some(dir.join(file_name(&client)));
        if let Err(e) = std::fs::create_dir_all(&dir) {
            s.load_error = Some(format!("{}: {e}", dir.display()));
            return s;
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(live_lock_name(&client)))
        {
            Ok(f) => match f.try_lock() {
                Ok(()) => s.live_lock = Some(f),
                Err(e) => {
                    s.load_error = Some(format!("client id {client} already in use: {e}"));
                    return s;
                }
            },
            Err(e) => {
                s.load_error = Some(format!("{}: {e}", dir.display()));
                return s;
            }
        }
        // Our own file (normally absent for a fresh id).
        if let Some(path) = s.path.clone() {
            match read_ops(&path) {
                Ok(ops) => s.ops = ops,
                Err(e) => s.load_error = Some(e),
            }
        }
        s.adopt_orphans();
        s
    }

    fn dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }

    /// Merge every orphaned file in the directory (a client that no longer holds its liveness
    /// lock, or the legacy shared file) into ours, then remove it. Live clients' files are never
    /// touched. Runs under the directory lock so two restarting clients don't both adopt.
    /// Returns how many operations were adopted.
    pub fn adopt_orphans(&mut self) -> usize {
        let Some(dir) = self.dir().map(Path::to_path_buf) else {
            return 0;
        };
        if self.live_lock.is_none() {
            return 0;
        }
        let Some(me) = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_owned())
        else {
            return 0;
        };
        let lock = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(DIR_LOCK))
        {
            Ok(f) => f,
            Err(_) => return 0,
        };
        if lock.lock().is_err() {
            return 0;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return 0;
        };
        let mut adopted_files: Vec<(PathBuf, Option<PathBuf>)> = Vec::new();
        let mut incoming: Vec<PendingOp> = Vec::new();
        for ent in rd.flatten() {
            let name = ent.file_name();
            let Some(n) = name.to_str() else { continue };
            if name == me {
                continue;
            }
            let live = if n == LEGACY {
                None
            } else if let Some(client) =
                n.strip_prefix(PREFIX).and_then(|r| r.strip_suffix(".json"))
            {
                let lp = dir.join(live_lock_name(client));
                // Someone still holds it: a live client — leave its entries alone.
                if let Ok(f) = std::fs::OpenOptions::new().write(true).open(&lp) {
                    match f.try_lock() {
                        Ok(()) => {}
                        Err(std::fs::TryLockError::WouldBlock) => continue,
                        Err(_) => continue,
                    }
                    // Our probe lock is released when `f` drops at the end of this iteration;
                    // the directory lock keeps other adopters out meanwhile.
                }
                Some(lp)
            } else {
                continue;
            };
            match read_ops(&ent.path()) {
                Ok(ops) => {
                    incoming.extend(ops);
                    adopted_files.push((ent.path(), live));
                }
                Err(e) => self.load_error = Some(e),
            }
        }
        let mut n = 0;
        for op in incoming {
            if self.ops.iter().all(|o| o.key != op.key) {
                // Adopted entries were dispatched by a dead connection: they need asking.
                self.ops.push(op);
                n += 1;
            }
        }
        if adopted_files.is_empty() {
            return 0;
        }
        // Write ours first; only then drop the orphans (a crash in between leaves duplicates,
        // which the next merge dedupes by key — never a lost key).
        if n > 0 && self.save().is_err() {
            self.ops.truncate(self.ops.len() - n);
            return 0;
        }
        for (f, live) in adopted_files {
            let _ = std::fs::remove_file(&f);
            if let Some(l) = live {
                let _ = std::fs::remove_file(l);
            }
        }
        self.adopted += n;
        n
    }

    /// Tests: adopt until `want` operations have been adopted in total. A concurrent fork in
    /// another test thread briefly inherits a dead client's lock descriptor (until its exec), so
    /// that client can look alive for a moment — the safe direction; the real client simply
    /// adopts on its next reconnect.
    #[cfg(test)]
    pub fn adopt_settled(&mut self, want: usize) {
        for _ in 0..400 {
            if self.adopted >= want {
                return;
            }
            self.adopt_orphans();
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn save(&self) -> std::io::Result<()> {
        let Some(path) = &self.path else {
            return Ok(());
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let seq = self.tmp_seq.get() + 1;
        self.tmp_seq.set(seq);
        let tmp = tmp_path(path, seq);
        let data = serde_json::to_vec_pretty(&self.ops).map_err(std::io::Error::other)?;
        let res = (|| {
            use std::io::Write;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(&data)?;
            f.sync_all()?;
            std::fs::rename(&tmp, path)
        })();
        if res.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        res
    }

    /// Persist `op` before dispatch. On error nothing is recorded and the caller must not send.
    pub fn add(&mut self, op: PendingOp) -> std::io::Result<()> {
        let key = op.key.clone();
        self.ops.retain(|o| o.key != key);
        self.ops.push(op);
        if let Err(e) = self.save() {
            self.ops.retain(|o| o.key != key);
            return Err(e);
        }
        self.live.insert(key);
        Ok(())
    }

    pub fn get(&self, key: &str) -> Option<&PendingOp> {
        self.ops.iter().find(|o| o.key == key)
    }

    /// The owner answered (success or a definitive error): forget the operation.
    pub fn remove(&mut self, key: &str) {
        self.live.remove(key);
        self.reconciling.remove(key);
        let before = self.ops.len();
        self.ops.retain(|o| o.key != key);
        if self.ops.len() != before {
            let _ = self.save();
        }
    }

    pub fn mark_unknown(&mut self, key: &str, note: impl Into<String>) {
        self.reconciling.remove(key);
        self.live.remove(key);
        if let Some(o) = self.ops.iter_mut().find(|o| o.key == key) {
            o.status = OpStatus::Unknown;
            o.note = Some(note.into());
        }
        let _ = self.save();
    }

    /// The link to `machine` dropped: anything dispatched over it now needs reconciliation.
    pub fn connection_lost(&mut self, machine: &str) {
        let keys: Vec<String> = self
            .ops
            .iter()
            .filter(|o| o.machine == machine)
            .map(|o| o.key.clone())
            .collect();
        for k in keys {
            self.live.remove(&k);
            self.reconciling.remove(&k);
        }
    }

    /// Operations for `machine` whose outcome must be asked for (not live, not already asked,
    /// and not already known to be unknown).
    pub fn needs_reconcile(&self, machine: &str) -> Vec<String> {
        self.ops
            .iter()
            .filter(|o| {
                o.machine == machine
                    && o.status == OpStatus::InFlight
                    && !self.live.contains(&o.key)
                    && !self.reconciling.contains(&o.key)
            })
            .map(|o| o.key.clone())
            .collect()
    }

    pub fn unknown_count(&self) -> usize {
        self.ops
            .iter()
            .filter(|o| o.status == OpStatus::Unknown)
            .count()
    }

    pub fn push_outcome(&mut self, s: String) {
        self.outcomes.push(s);
        if self.outcomes.len() > 10 {
            self.outcomes.remove(0);
        }
    }
}

impl Drop for PendingStore {
    /// A clean exit with nothing pending leaves no file behind; anything still pending stays for
    /// this client's next run or another client to adopt.
    fn drop(&mut self) {
        if self.live_lock.is_some()
            && self.ops.is_empty()
            && let Some(path) = &self.path
        {
            let _ = std::fs::remove_file(path);
            if let (Some(dir), Some(name)) =
                (path.parent(), path.file_name().and_then(|n| n.to_str()))
                && let Some(client) = name
                    .strip_prefix(PREFIX)
                    .and_then(|r| r.strip_suffix(".json"))
            {
                let _ = std::fs::remove_file(dir.join(live_lock_name(client)));
            }
        }
    }
}

pub fn read_ops(path: &Path) -> Result<Vec<PendingOp>, String> {
    match std::fs::read(path) {
        Ok(b) => serde_json::from_slice::<Vec<PendingOp>>(&b)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// A temp name unique to this process and write, so concurrent writers never share one.
fn tmp_path(p: &Path, seq: u64) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(format!(".{}.{seq}.tmp", std::process::id()));
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn op(key: &str, machine: &str) -> PendingOp {
        PendingOp {
            key: key.into(),
            method: "task.track".into(),
            params: json!({"run": "r1", "title": "Fix login", "idempotency_key": key}),
            machine: machine.into(),
            created_at_ms: 1,
            status: OpStatus::InFlight,
            note: None,
        }
    }

    fn keys(s: &PendingStore) -> Vec<String> {
        let mut k: Vec<String> = s.ops.iter().map(|o| o.key.clone()).collect();
        k.sort();
        k
    }

    #[test]
    fn persisted_before_dispatch_and_adopted_after_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().join("s");
        let path = d.join("client-pending-c1.json");
        {
            let mut s = PendingStore::open(d.clone(), "c1");
            s.add(op("k1", "devbox")).unwrap();
            assert!(path.exists());
            // Live in this session: not reconciled while a response may still arrive.
            assert!(s.needs_reconcile("devbox").is_empty());
            // Crash: dropped with the entry still pending.
        }
        let mut s2 = PendingStore::open(d.clone(), "c2");
        s2.adopt_settled(1);
        assert_eq!(s2.adopted, 1);
        assert_eq!(keys(&s2), vec!["k1"]);
        assert_eq!(s2.needs_reconcile("devbox"), vec!["k1".to_string()]);
        assert!(s2.needs_reconcile("other").is_empty());
        // The orphan's file is gone; the entry now lives in c2's file.
        assert!(!path.exists());
        assert!(d.join("client-pending-c2.json").exists());
        s2.remove("k1");
        drop(s2);
        // A clean exit with nothing pending leaves nothing to adopt.
        assert!(PendingStore::open(d, "c3").ops.is_empty());
    }

    /// Codex G02 #15: two clients in one session never overwrite each other, a live client's
    /// entries are never adopted, and a crashed client's entries survive with their keys.
    #[test]
    fn two_live_clients_keep_their_own_entries_and_adopt_after_crash() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        let mut a = PendingStore::open(d.clone(), "tui-1-a");
        let mut b = PendingStore::open(d.clone(), "tui-2-b");
        a.add(op("accept-a", "m")).unwrap();
        b.add(op("track-b", "m")).unwrap();
        b.add(op("track-b2", "m")).unwrap();
        b.remove("track-b2");
        // B's writes didn't erase A's record (and vice versa).
        let a_disk = read_ops(&d.join("client-pending-tui-1-a.json")).unwrap();
        assert_eq!(a_disk.len(), 1);
        assert_eq!(a_disk[0].key, "accept-a");
        assert_eq!(
            read_ops(&d.join("client-pending-tui-2-b.json")).unwrap()[0].key,
            "track-b"
        );
        // While both are alive, neither adopts the other's entries.
        assert_eq!(b.adopt_orphans(), 0);
        assert_eq!(a.adopt_orphans(), 0);
        let c = PendingStore::open(d.clone(), "tui-3-c");
        assert!(c.ops.is_empty());
        assert!(d.join("client-pending-tui-1-a.json").exists());
        // A crashes: its original reconciliation key survives — B picks it up on reconnect.
        drop(c);
        drop(a);
        b.adopt_settled(1);
        assert_eq!(b.adopted, 1);
        assert_eq!(keys(&b), vec!["accept-a", "track-b"]);
        assert!(!d.join("client-pending-tui-1-a.json").exists());
        let on_disk = read_ops(&d.join("client-pending-tui-2-b.json")).unwrap();
        assert_eq!(on_disk.len(), 2);
        // Adopted entries need asking; B's own live dispatch doesn't.
        assert_eq!(b.needs_reconcile("m"), vec!["accept-a".to_string()]);
    }

    #[test]
    fn legacy_shared_file_is_adopted_and_duplicates_merge_by_key() {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path().to_path_buf();
        std::fs::write(
            d.join("client-pending.json"),
            serde_json::to_vec(&vec![op("x", "m"), op("y", "m")]).unwrap(),
        )
        .unwrap();
        // A crash between writing the adopter's file and deleting the orphan leaves duplicates.
        std::fs::write(
            d.join("client-pending-dead.json"),
            serde_json::to_vec(&vec![op("y", "m")]).unwrap(),
        )
        .unwrap();
        let s = PendingStore::open(d.clone(), "new");
        assert_eq!(keys(&s), vec!["x", "y"]);
        assert!(!d.join("client-pending.json").exists());
        assert!(!d.join("client-pending-dead.json").exists());
    }

    #[test]
    fn writes_use_unique_temp_names_and_leave_none_behind() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("client-pending-c.json");
        assert_ne!(tmp_path(&p, 1), tmp_path(&p, 2));
        let mut s = PendingStore::open(dir.path().to_path_buf(), "c");
        s.add(op("a", "m")).unwrap();
        s.add(op("b", "m")).unwrap();
        let tmps = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(tmps, 0);
    }

    #[test]
    fn unknown_is_not_reconciled_again_and_link_loss_requeues() {
        let mut s = PendingStore::default();
        s.add(op("a", "m")).unwrap();
        s.add(op("b", "m")).unwrap();
        s.connection_lost("m");
        assert_eq!(s.needs_reconcile("m").len(), 2);
        s.mark_unknown("a", "no receipt");
        assert_eq!(s.needs_reconcile("m"), vec!["b".to_string()]);
        assert_eq!(s.unknown_count(), 1);
    }

    #[test]
    fn failed_persist_records_nothing() {
        let dir = tempfile::tempdir().unwrap();
        // A file where the directory should be makes create_dir_all fail.
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"x").unwrap();
        let mut s = PendingStore::open(blocker.clone(), "c");
        assert!(s.load_error.is_some());
        assert!(s.add(op("k", "m")).is_err());
        assert!(s.ops.is_empty());
        assert!(s.live.is_empty());
    }
}
