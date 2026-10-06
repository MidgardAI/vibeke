//! Per-plugin runtime limits (07 §7.7): settings from `[plugins]`, the concurrency gate with a
//! bounded hook queue, and the log ring caps with truncation markers.

use super::*;
use herdr::settings::{MAX_QUEUED, PluginSettings};

/// Where the last `[plugins]` section that loaded is kept (outside every plugin-visible path:
/// the state root is hidden from sandboxed plugins).
fn last_good_path() -> PathBuf {
    let dirs = plugin_dirs();
    dirs.state
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or(dirs.state)
        .join("last-good-settings.json")
}

/// How `config.toml` looked at the last read.
#[derive(Clone)]
enum Loaded {
    /// It loaded (`None`: no `[plugins]` section).
    Ok(Option<toml::Value>),
    /// The file does not exist.
    Missing,
    /// It exists but cannot be read or parsed.
    Failed(String),
}

/// Settings of plugin `id`, re-read when `config.toml` changes. **Fails closed:** when the
/// configuration cannot be read or parsed (or has disappeared), the last `[plugins]` section
/// that loaded (kept in memory and persisted next to the plugin state) decides, and a plugin it
/// configured with `isolate = "sandbox"` is refused ([`PluginSettings::error`]) rather than run
/// on the host; with no known-good section at all, a broken file refuses every plugin. An
/// unknown `isolate` value is refused by [`PluginSettings::from_toml`].
pub fn settings(id: &str) -> PluginSettings {
    // mtime, ctime, size, mode, inode: a chmod or an in-place rewrite changes the key.
    type Key = Option<(i64, i64, i64, i64, u64, u32, u64)>;
    type Cached = Option<(Key, Loaded)>;
    static CACHE: LazyLock<Mutex<Cached>> = LazyLock::new(|| Mutex::new(None));
    static LAST_GOOD: LazyLock<Mutex<Option<Option<toml::Value>>>> =
        LazyLock::new(|| Mutex::new(None));
    let path = vk_config::config_path();
    let key: Key = std::fs::metadata(&path).ok().map(|m| {
        use std::os::unix::fs::MetadataExt;
        (
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
            m.len(),
            m.mode(),
            m.ino(),
        )
    });
    let loaded = {
        let mut c = CACHE.lock().unwrap();
        if c.as_ref().is_none_or(|(k, _)| *k != key) {
            let l = if key.is_none() && !path.exists() {
                Loaded::Missing
            } else {
                match vk_config::Config::load(&path) {
                    Ok((cfg, _)) => Loaded::Ok(cfg.extra.get("plugins").cloned()),
                    Err(e) => Loaded::Failed(e.to_string()),
                }
            };
            *c = Some((key, l));
        }
        c.as_ref()
            .map(|(_, l)| l.clone())
            .unwrap_or(Loaded::Missing)
    };
    match loaded {
        Loaded::Ok(v) => {
            let mut lg = LAST_GOOD.lock().unwrap();
            if lg.as_ref() != Some(&v) {
                let json = serde_json::to_vec(&v).unwrap_or_default();
                let file = last_good_path();
                let tmp = file.with_extension("json.tmp");
                let saved = file
                    .parent()
                    .map_or(Ok(()), std::fs::create_dir_all)
                    .and_then(|_| std::fs::write(&tmp, &json))
                    .and_then(|_| std::fs::rename(&tmp, &file));
                if let Err(e) = saved {
                    tracing::warn!(error = %e, "plugin settings: last known-good copy not saved");
                }
                *lg = Some(v.clone());
            }
            PluginSettings::from_toml(v.as_ref(), id)
        }
        Loaded::Missing | Loaded::Failed(_) => {
            let why = match &loaded {
                Loaded::Failed(e) => format!("config.toml cannot be loaded ({e})"),
                _ => "config.toml has disappeared".to_string(),
            };
            let remembered = LAST_GOOD.lock().unwrap().clone().or_else(|| {
                std::fs::read(last_good_path())
                    .ok()
                    .and_then(|b| serde_json::from_slice::<Option<toml::Value>>(&b).ok())
            });
            match remembered {
                Some(v) => {
                    let mut s = PluginSettings::from_toml(v.as_ref(), id);
                    if s.isolate == herdr::settings::Isolate::Sandbox {
                        s.error = Some(format!(
                            "{why}; {id} was last configured with isolate = \"sandbox\" and is not run until the configuration loads again"
                        ));
                    } else {
                        s.warnings
                            .push(format!("{why}; using the last settings that loaded"));
                    }
                    s
                }
                None if matches!(loaded, Loaded::Missing) => PluginSettings::default(),
                None => PluginSettings {
                    error: Some(format!(
                        "{why}; plugins are not run until it loads (their isolation settings are unknown)"
                    )),
                    ..PluginSettings::default()
                },
            }
        }
    }
}

/// Ring caps of one invocation's output streams.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogCaps {
    pub bytes: usize,
    pub lines: usize,
}

impl LogCaps {
    pub fn of(s: &PluginSettings) -> Self {
        LogCaps {
            bytes: s.log_max_bytes,
            lines: s.log_max_lines,
        }
    }
}

/// The line that replaces output that fell off the front of a ring.
pub fn truncation_marker(dropped: u64) -> String {
    format!("[vibeke: {dropped} earlier bytes of output truncated]\n")
}

/// Keep at most `cap` records of `plugin` (oldest first out; running ones are kept).
pub fn trim_records(logs: &mut VecDeque<Value>, plugin: &str, cap: usize) {
    let of_plugin = |l: &Value| l["plugin_id"] == plugin;
    let mut over = logs
        .iter()
        .filter(|l| of_plugin(l))
        .count()
        .saturating_sub(cap);
    if over == 0 {
        return;
    }
    logs.retain(|l| {
        if over > 0 && of_plugin(l) && l["status"] != "running" {
            over -= 1;
            false
        } else {
            true
        }
    });
}

/// One running (action or hook) invocation's claim on its plugin's concurrency limit.
pub struct Slot {
    st: Arc<State>,
    plugin: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut r = self.st.running.lock().unwrap();
        if let Some(n) = r.get_mut(&self.plugin) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                r.remove(&self.plugin);
            }
        }
    }
}

/// Claim a slot, or `None` when the plugin already runs `max_concurrent` invocations.
pub fn try_slot(server: &Server, plugin: &str) -> Option<Slot> {
    let max = settings(plugin).max_concurrent;
    let st = state(server);
    {
        let mut r = st.running.lock().unwrap();
        let n = r.entry(plugin.to_string()).or_insert(0);
        if *n >= max {
            return None;
        }
        *n += 1;
    }
    Some(Slot {
        st,
        plugin: plugin.to_string(),
    })
}

pub fn busy(plugin: &str) -> WireError {
    let max = settings(plugin).max_concurrent;
    WireError::new(
        "busy",
        format!(
            "{plugin} already has {max} running invocations; try again when one finishes (`[plugins.\"{plugin}\"] max_concurrent`)"
        ),
    )
}

/// An event-hook invocation waiting for a free slot.
pub struct Queued {
    pub entry: Entry,
    pub m: Manifest,
    pub command: Vec<String>,
    pub source: String,
    pub event: Option<(String, Value)>,
    pub entrypoint: Option<String>,
    pub ctx: InvokeContext,
}

fn start(server: &Arc<Server>, q: Queued, slot: Slot) {
    let sp = Spawn {
        source: &q.source,
        action: None,
        event: q.event.clone(),
        entrypoint: q.entrypoint.clone(),
        ctx: q.ctx.clone(),
        long_lived: false,
    };
    spawn_invocation(server, &q.entry, &q.m, &q.command, sp, Some(slot));
}

/// Run a hook now, queue it behind the plugin's running invocations, or (queue full) record it
/// as failed with `busy`.
pub fn dispatch_hook(server: &Arc<Server>, q: Queued) {
    if let Some(slot) = try_slot(server, &q.entry.id) {
        start(server, q, slot);
        return;
    }
    let st = state(server);
    let plugin = q.entry.id.clone();
    {
        let mut queues = st.queues.lock().unwrap();
        let dq = queues.entry(plugin.clone()).or_default();
        if dq.len() < MAX_QUEUED {
            dq.push_back(q);
            return;
        }
    }
    let n = st.next.fetch_add(1, Ordering::Relaxed) + 1;
    let now = now_ms();
    push_log(
        server,
        json!({
            "log_id": format!("l{n}-{}", &crate::core::ulid()[20..]),
            "plugin_id": plugin,
            "event": q.event.as_ref().map(|e| e.0.clone()),
            "entrypoint_id": q.entrypoint,
            "source": q.source,
            "command": q.command,
            "status": "failed",
            "error": "busy",
            "started_unix_ms": now, "started_at": now,
            "finished_unix_ms": now, "finished_at": now,
            "exit_code": null,
            "stdout": "",
            "stderr": format!("busy: {} hooks already queued for {plugin}; dropped", MAX_QUEUED),
        }),
    );
}

/// Start queued hooks while the plugin has free slots (called when an invocation ends).
pub fn drain(server: &Arc<Server>, plugin: &str) {
    loop {
        let Some(slot) = try_slot(server, plugin) else {
            return;
        };
        let next = state(server)
            .queues
            .lock()
            .unwrap()
            .get_mut(plugin)
            .and_then(VecDeque::pop_front);
        match next {
            Some(q) => start(server, q, slot),
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rings_keep_the_tail_and_say_so() {
        let caps = LogCaps {
            bytes: 1000,
            lines: 3,
        };
        let mut b = b"a\nb\nc\nd\ne\n".to_vec();
        let dropped = tail_keep(&mut b, caps);
        assert_eq!(b, b"c\nd\ne\n");
        assert_eq!(dropped, 4);
        let text = redacted(&b, dropped);
        assert!(text.starts_with("[vibeke: 4 earlier bytes of output truncated]\n"));
        assert!(text.ends_with("c\nd\ne\n"));
        assert_eq!(redacted(&b, 0), "c\nd\ne\n");
        let mut big = vec![b'x'; 150];
        let caps = LogCaps {
            bytes: 100,
            lines: 10,
        };
        assert_eq!(tail_keep(&mut big, caps), 50);
        assert_eq!(big.len(), 100);
    }

    #[test]
    fn per_plugin_record_ring() {
        let mk = |p: &str, id: u32, st: &str| json!({"plugin_id": p, "log_id": id, "status": st});
        let mut l: VecDeque<Value> = VecDeque::new();
        for i in 0..5 {
            l.push_back(mk("a", i, "succeeded"));
            l.push_back(mk("b", 100 + i, "succeeded"));
        }
        trim_records(&mut l, "a", 2);
        let a: Vec<_> = l.iter().filter(|x| x["plugin_id"] == "a").collect();
        assert_eq!(a.len(), 2);
        assert_eq!(a[0]["log_id"], 3, "oldest dropped first");
        assert_eq!(l.iter().filter(|x| x["plugin_id"] == "b").count(), 5);
    }
}
