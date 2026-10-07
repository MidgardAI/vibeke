//! Container templates and the warm pool (13 §9).
//!
//! **Templates** (`[isolation.container] template = true`): once a box's trusted setup steps
//! (devcontainer lifecycle commands, `.vibeke/setup.sh`) succeed, the box is committed as
//! `vibeke-template:<key>`, keyed by provider, image and the exact steps. A later box with the
//! same key starts from that image and skips the steps (`sandbox.template_used`). Only what the
//! steps changed inside the image is cached: bind-mounted dirs (the clone at `/workspace`, the
//! private `$HOME`) are not, named cache volumes keep package caches.
//!
//! **Warm pool** (`warm_pool = N`, `warm_ttl`): after a container task was created, N more boxes
//! with the same inputs (repo, image/devcontainer/repo config, network, harnesses, yolo and the
//! `[isolation.container]` config) are created in the background under `pool:<key>:<n>` slots,
//! running, with credentials projected and no clone yet. The next task with the same inputs
//! claims one: its context adopts the slot's box and host dirs (the slot stays its storage
//! name), the private clone is made at claim time and setup runs then unless a template already
//! covered it. Slots older than `warm_ttl` are removed. Credentials in a warm box are the ones
//! projected at warm-up (refreshed when the slot is recycled).

use super::*;
use std::collections::HashSet;
use vk_sandbox::boxops;

const KV_TEMPLATE: &str = "sandbox_template";
const KV_POOL: &str = "sandbox_pool";
const POOL_CHECK: Duration = Duration::from_secs(60);

#[derive(Default)]
pub struct PoolState {
    inner: Mutex<PoolInner>,
    /// Tests: runs inside a slot update right after the list was read.
    #[cfg(test)]
    pub after_read: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}

#[derive(Default)]
struct PoolInner {
    /// Pool keys with a refill in flight.
    filling: HashSet<String>,
    last_check: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Slot {
    pub slot: String,
    pub pool: String,
    pub created_ms: i64,
}

fn slots(server: &Server) -> Vec<Slot> {
    server
        .with_core(|c| c.store.kv_get(KV_POOL, "slots").ok().flatten())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Read, change and save the slot list as one step under the core lock (one transaction):
/// two concurrent claims can never both take the same slot, and a refill or recycle never
/// overwrites a claim made in between. `f` returns what the caller gets; the list is saved
/// only when it changed.
fn update_slots<T>(server: &Server, f: impl FnOnce(&mut Vec<Slot>) -> T) -> T {
    let mut c = server.core.lock().unwrap();
    let mut v: Vec<Slot> = c
        .store
        .kv_get(KV_POOL, "slots")
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    let before = v.clone();
    #[cfg(test)]
    if let Some(hook) = server.sandbox.pool.after_read.lock().unwrap().as_ref() {
        hook();
    }
    let out = f(&mut v);
    if v != before {
        let mut tx = Tx::new();
        tx.m.kv(
            KV_POOL,
            "slots",
            Some(serde_json::to_string(&v).unwrap_or_default()),
        );
        let _ = server.commit(&mut c, tx);
    }
    out
}

/// Container names of unclaimed warm boxes (never pruned).
pub fn warm_names(server: &Server) -> HashSet<String> {
    slots(server)
        .iter()
        .map(|s| format!("vk-{}", vk_sandbox::runner::short_id(&s.slot)))
        .collect()
}

/// Container names of boxes that live contexts adopted from the pool.
pub fn claimed_names(server: &Server) -> HashSet<String> {
    server
        .sandbox
        .inner
        .lock()
        .unwrap()
        .boxes
        .values()
        .filter_map(|b| b.request.slot.as_ref())
        .map(|s| format!("vk-{}", vk_sandbox::runner::short_id(s)))
        .collect()
}

/// Storage ids of every slot (warm or claimed): their dirs are not orphans.
pub fn slot_ids(server: &Server) -> HashSet<String> {
    let mut v: HashSet<String> = slots(server)
        .iter()
        .map(|s| vk_sandbox::runner::short_id(&s.slot))
        .collect();
    v.extend(
        server
            .sandbox
            .inner
            .lock()
            .unwrap()
            .boxes
            .values()
            .filter_map(|b| b.request.slot.as_ref())
            .map(|s| vk_sandbox::runner::short_id(s)),
    );
    v
}

// ---- templates --------------------------------------------------------------------------------

/// A box's template: its key and whether it started from the committed image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateUse {
    pub key: String,
    pub from_template: bool,
}

/// Decide the template for a box being built (blocking: may run `image inspect`). With a usable
/// committed template the image is swapped for it and the setup steps are dropped.
pub fn apply_template(
    server: &Server,
    cfg: &IsolationConfig,
    provider: &vk_sandbox::container::Provider,
    cli_env: &[(String, String)],
    image: &mut String,
    steps: &mut Vec<(String, String)>,
) -> Option<TemplateUse> {
    if !cfg.container.template || steps.is_empty() || provider.name() == "apple-container" {
        return None;
    }
    let key = boxops::template_key(provider.name(), image, steps);
    let recorded = server
        .with_core(|c| c.store.kv_get(KV_TEMPLATE, &key).ok().flatten())
        .is_some();
    let tag = boxops::template_tag(&key);
    let exists = || {
        let argv = vec![
            provider.cli().to_string_lossy().into_owned(),
            "image".into(),
            "inspect".into(),
            "--format".into(),
            "{{.Id}}".into(),
            tag.clone(),
        ];
        vk_sandbox::container::run_cmd(&argv, cli_env, None, Duration::from_secs(20))
            .is_ok_and(|o| o.ok && !o.stdout.trim().is_empty())
    };
    if recorded && exists() {
        *image = tag;
        steps.clear();
        return Some(TemplateUse {
            key,
            from_template: true,
        });
    }
    Some(TemplateUse {
        key,
        from_template: false,
    })
}

/// The box's setup succeeded: commit it as the template (blocking).
pub fn after_setup(
    server: &Server,
    key: &str,
    task: Option<&str>,
    b: &vk_sandbox::container::ContainerBox,
    template: &str,
) {
    let tag = boxops::template_tag(template);
    match b.commit(&tag) {
        Ok(()) => {
            let mut c = server.core.lock().unwrap();
            let mut tx = Tx::new();
            tx.m.kv(
                KV_TEMPLATE,
                template,
                Some(
                    json!({"image": tag, "at_ms": vk_store::now_ms(), "from": b.spec.image})
                        .to_string(),
                ),
            );
            tx.event(
                "sandbox.template_created",
                json!({"task": task, "sandbox": key}),
                json!({"template": template, "image": tag}),
            );
            let _ = server.commit(&mut c, tx);
        }
        Err(e) => tracing::warn!(template, error = %e, "template commit failed"),
    }
}

// ---- warm pool --------------------------------------------------------------------------------

/// The pool key of a container task request (`None` when the pool is off or the box would not
/// be a task clone box). Every input that shapes the box is part of it.
pub fn pool_key(cfg: &IsolationConfig, checkout: &Path, req: &IsoRequest) -> Option<String> {
    if cfg.container.warm_pool == 0 || req.level != IsolationLevel::Container {
        return None;
    }
    if super::container::code_mode(req.code.as_deref(), cfg, true) != "clone" {
        return None;
    }
    let repo = vk_tasks::repo_root(checkout)?;
    let mut h = blake3::Hasher::new();
    let mut harnesses = req.harnesses.clone();
    harnesses.sort();
    let mut feed = |b: &[u8]| {
        h.update(b);
        h.update(b"\0");
    };
    feed(repo.root.to_string_lossy().as_bytes());
    feed(req.image.as_deref().unwrap_or("").as_bytes());
    feed(req.network.as_str().as_bytes());
    feed(req.devcontainer.as_deref().unwrap_or("").as_bytes());
    feed(&[req.build as u8, req.yolo as u8]);
    feed(harnesses.join(",").as_bytes());
    feed(
        serde_json::to_string(&cfg.container)
            .unwrap_or_default()
            .as_bytes(),
    );
    for f in [
        ".vibeke/sandbox.toml",
        ".vibeke/setup.sh",
        ".devcontainer/devcontainer.json",
        ".devcontainer.json",
    ] {
        feed(&std::fs::read(checkout.join(f)).unwrap_or_default());
    }
    if let Some(d) = &req.devcontainer {
        feed(&std::fs::read(checkout.join(d)).unwrap_or_default());
    }
    Some(h.finalize().to_hex()[..12].to_string())
}

/// Take a warm slot for pool `key` (fresh ones only). Atomic: read, remove and save happen in
/// one step, so a slot is handed to exactly one claimer.
pub fn claim(server: &Server, key: &str, ttl: Duration) -> Option<String> {
    let now = vk_store::now_ms();
    update_slots(server, |v| {
        let i = v
            .iter()
            .position(|s| s.pool == key && now - s.created_ms < ttl.as_millis() as i64)?;
        Some(v.remove(i).slot)
    })
}

/// The context a slot was claimed for.
pub fn claimed(server: &Server, key: &str, task: Option<&str>, slot: &str) {
    emit(
        server,
        "sandbox.warm_claimed",
        json!({"task": task, "sandbox": key}),
        json!({"slot": slot}),
    );
}

/// Top pool `key` up to `[isolation.container] warm_pool` slots, in the background. `template`
/// is the request and checkout of the box that just used the pool key.
pub fn refill(server: &Arc<Server>, key: &str, checkout: &Path, req: &IsoRequest) {
    let cfg = super::extras::cfg(server);
    let want = cfg.container.warm_pool as usize;
    if want == 0 || tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    {
        let mut i = server.sandbox.pool.inner.lock().unwrap();
        if !i.filling.insert(key.to_string()) {
            return;
        }
    }
    let have = slots(server).iter().filter(|s| s.pool == key).count();
    let (srv, key, checkout, req) = (
        server.clone(),
        key.to_string(),
        checkout.to_path_buf(),
        req.clone(),
    );
    tokio::spawn(async move {
        for n in have..want {
            let slot = format!("pool:{key}:{}", &ulid()[16..]);
            match warm_up(&srv, &slot, &checkout, &req, &cfg).await {
                Ok(()) => {
                    update_slots(&srv, |v| {
                        v.push(Slot {
                            slot: slot.clone(),
                            pool: key.clone(),
                            created_ms: vk_store::now_ms(),
                        })
                    });
                    emit(
                        &srv,
                        "sandbox.warm_ready",
                        json!({"sandbox": slot}),
                        json!({"pool": key, "index": n}),
                    );
                }
                Err(e) => {
                    tracing::warn!(pool = %key, error = %e.message, "warm box not created");
                    break;
                }
            }
        }
        srv.sandbox.pool.inner.lock().unwrap().filling.remove(&key);
    });
}

/// Create one warm box for `slot`: projected credentials, the box running, no clone, no setup.
async fn warm_up(
    server: &Arc<Server>,
    slot: &str,
    checkout: &Path,
    req: &IsoRequest,
    cfg: &IsolationConfig,
) -> Result<(), vk_proto::rpc::RpcError> {
    let home = server.sandbox.home();
    let root = sbx_root(slot);
    std::fs::create_dir_all(&root).map_err(internal)?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700));
    }
    let box_wd = PathBuf::from(super::container::workdir("clone", checkout, None));
    let trust = req.yolo.then_some(box_wd.as_path());
    let projection =
        project_all(server, &req.harnesses, &root.join("shared"), trust).map_err(internal)?;
    let (srv, slot2, co, cfg2, req2, home2) = (
        server.clone(),
        slot.to_string(),
        checkout.to_path_buf(),
        cfg.clone(),
        req.clone(),
        home.clone(),
    );
    tokio::task::spawn_blocking(move || {
        let mut c = super::container::build(super::container::BuildIn {
            server: &srv,
            key: &slot2,
            task: Some(&slot2),
            checkout: &co,
            cfg: &cfg2,
            network: req2.network,
            image: req2.image.clone(),
            code: "clone",
            devcontainer: req2.devcontainer.clone(),
            build_image: req2.build,
            projection: &projection,
            home: &home2,
            protected: &[],
        })?;
        // No task branch yet and nothing to set up in an empty workspace: both at claim time.
        c.clone = None;
        c.lifecycle.clear();
        c.runner
            .check()
            .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
        c.b()
            .ensure_running()
            .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
        Ok::<_, vk_proto::rpc::RpcError>(())
    })
    .await
    .map_err(internal)?
}

/// Recycle slots older than `warm_ttl` (from the extras tick; throttled to once a minute).
pub fn check(server: &Arc<Server>) {
    let cfg = super::extras::cfg(server);
    {
        let mut i = server.sandbox.pool.inner.lock().unwrap();
        if i.last_check.is_some_and(|t| t.elapsed() < POOL_CHECK) {
            return;
        }
        i.last_check = Some(Instant::now());
    }
    let ttl = cfg.warm_ttl().as_millis() as i64;
    let now = vk_store::now_ms();
    let old: Vec<Slot> = update_slots(server, |v| {
        let (old, keep): (Vec<Slot>, Vec<Slot>) = std::mem::take(v)
            .into_iter()
            .partition(|s| now - s.created_ms >= ttl || cfg.container.warm_pool == 0);
        *v = keep;
        old
    });
    if old.is_empty() {
        return;
    }
    let runtime = server
        .sandbox
        .container_runtime()
        .map(|p| p.to_string_lossy().into_owned())
        .or(cfg.container.runtime.clone());
    let env = vk_sandbox::container::cli_env(&server.opts.env);
    let srv = server.clone();
    std::thread::spawn(move || {
        let provider = match runtime.as_deref() {
            Some(r) => vk_sandbox::container::Provider::from_config(r),
            None => vk_sandbox::container::detect(),
        };
        for s in old {
            if let Some(p) = &provider {
                let name = format!("vk-{}", vk_sandbox::runner::short_id(&s.slot));
                let argv = vec![
                    p.cli().to_string_lossy().into_owned(),
                    "rm".into(),
                    "--force".into(),
                    name,
                ];
                let _ = vk_sandbox::container::run_cmd(&argv, &env, None, Duration::from_secs(60));
            }
            let _ = std::fs::remove_dir_all(sbx_root(&s.slot));
            let _ = std::fs::remove_dir_all(super::container::run_dir(&s.slot));
            emit(
                &srv,
                "sandbox.warm_recycled",
                json!({"sandbox": s.slot}),
                json!({"pool": s.pool}),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Final review P1 10: two task creations claim the one warm slot at the same time (the
    /// second starts while the first sits between reading and saving the list). Exactly one
    /// gets the slot; the other gets none and its box is named after its own task, so the two
    /// tasks never share a container or storage.
    #[test]
    fn concurrent_claims_of_one_slot_hand_it_out_once() {
        let dir = tempfile::tempdir().unwrap();
        let s = crate::hardening::testkit::server(dir.path(), "pool");
        update_slots(&s, |v| {
            v.push(Slot {
                slot: "pool:k1:0001".into(),
                pool: "k1".into(),
                created_ms: vk_store::now_ms(),
            })
        });
        // Widen the window between the read and the save.
        *s.sandbox.pool.after_read.lock().unwrap() = Some(Box::new(|| {
            std::thread::sleep(Duration::from_millis(150));
        }));
        let ttl = Duration::from_secs(600);
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let claims: Vec<Option<String>> = std::thread::scope(|sc| {
            let hs: Vec<_> = (0..2)
                .map(|_| {
                    let (s, b) = (s.clone(), barrier.clone());
                    sc.spawn(move || {
                        b.wait();
                        claim(&s, "k1", ttl)
                    })
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        *s.sandbox.pool.after_read.lock().unwrap() = None;
        let won: Vec<&String> = claims.iter().flatten().collect();
        assert_eq!(won.len(), 1, "{claims:?}");
        assert!(slots(&s).is_empty());
        // The two tasks' container names (slot-named vs task-named) differ.
        let tasks = ["task-a", "task-b"];
        let names: Vec<String> = claims
            .iter()
            .zip(tasks)
            .map(|(c, t)| {
                format!(
                    "vk-{}",
                    vk_sandbox::runner::short_id(c.as_deref().unwrap_or(t))
                )
            })
            .collect();
        assert_ne!(names[0], names[1]);
        // A refill landing after the claim keeps the claim (no stale overwrite).
        update_slots(&s, |v| {
            v.push(Slot {
                slot: "pool:k1:0002".into(),
                pool: "k1".into(),
                created_ms: vk_store::now_ms(),
            })
        });
        assert_eq!(slots(&s).len(), 1);
        assert_eq!(claim(&s, "k1", ttl).as_deref(), Some("pool:k1:0002"));
        assert_eq!(claim(&s, "k1", ttl), None);
    }
}
