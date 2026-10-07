//! The `vm` isolation level (13 §2.1, §9): `vm.*` and the box runner behind `--isolate vm`.
//!
//! Off by default (`[isolation.vm] enabled`). With `provider = "fake"` the whole path runs on
//! directory-backed VMs (no hardware, no network). Lima and Tart produce their real command
//! lines but have not been run against real VMs (that needs Apple Silicon); a real provider
//! refuses every network profile except `open`, because the host-to-VM link that enforces
//! `none`/proxy profiles for containers (`vk_remote::boxlink`) is not wired for VMs yet —
//! failing closed rather than pretending to filter.

use crate::Server;
use crate::api::{Ctx, R, b, err, internal, invalid, not_found, req, s, u};
use crate::orch;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use vk_proto::rpc::ErrorKind;
use vk_sandbox::NetworkProfile;
use vk_sandbox::vm::{
    self, FakeVmBackend, TemplateSpec, VmBackend, VmBoxRunner, VmManager, VmMount, VmProviderKind,
    VmSpec,
};
use vk_sandbox::vm_transport;

fn backend_overrides() -> &'static Mutex<HashMap<PathBuf, Arc<dyn VmBackend>>> {
    static O: OnceLock<Mutex<HashMap<PathBuf, Arc<dyn VmBackend>>>> = OnceLock::new();
    O.get_or_init(Mutex::default)
}

/// Use `b` as this server's VM backend (tests).
pub fn set_backend(server: &Server, b: Arc<dyn VmBackend>) {
    backend_overrides()
        .lock()
        .unwrap()
        .insert(server.paths.state.clone(), b);
}

pub fn vm_cfg(server: &Server) -> vk_sandbox::config::VmConfig {
    crate::sandbox::extras::cfg(server).vm
}

pub fn enabled(server: &Server) -> bool {
    vm_cfg(server).enabled
}

fn backend(server: &Server) -> Result<Arc<dyn VmBackend>, vk_proto::rpc::RpcError> {
    if let Some(b) = backend_overrides().lock().unwrap().get(&server.paths.state) {
        return Ok(b.clone());
    }
    vm::backend_for(&vm_cfg(server).provider, &server.paths.state)
        .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))
}

fn manager(server: &Server) -> Result<VmManager, vk_proto::rpc::RpcError> {
    Ok(VmManager::new(backend(server)?, &server.paths.state))
}

fn gate(server: &Server) -> Result<vk_sandbox::config::VmConfig, vk_proto::rpc::RpcError> {
    let c = vm_cfg(server);
    if !c.enabled {
        return Err(err(
            ErrorKind::Unsupported,
            "the vm level is off: set `[isolation.vm] enabled = true` in config.toml (provider tart, lima or fake)",
        )
        .details(json!({"reason": "feature_disabled", "flag": "isolation.vm.enabled"})));
    }
    Ok(c)
}

fn vm_err(e: vm::VmError) -> vk_proto::rpc::RpcError {
    match e {
        vm::VmError::NotFound(n) => not_found("vm", &n),
        vm::VmError::Invalid(m) => invalid(m),
        vm::VmError::Unavailable(m) => err(ErrorKind::Unsupported, m),
        vm::VmError::State { name, msg } => err(ErrorKind::Conflict, format!("vm {name}: {msg}")),
        other => err(ErrorKind::Internal, other.to_string()),
    }
}

/// `vk-<id suffix>` for a task/box key.
fn vm_name(key: &str) -> String {
    format!("vk-{}", vk_sandbox::runner::short_id(key))
}

fn spec_of(c: &vk_sandbox::config::VmConfig, name: &str, checkout: Option<&Path>) -> VmSpec {
    let mut s = VmSpec::new(name);
    s.image = c.image.clone();
    s.cpus = c.cpus;
    s.memory_mb = c.memory_mb();
    s.disk_gb = c.disk_gb();
    if let Some(co) = checkout {
        s.mounts.push(VmMount {
            host: co.to_path_buf(),
            target: vm::VM_WORKSPACE.into(),
            read_only: false,
        });
    }
    s
}

/// Read-only mounts over what host git trusts inside the writable checkout (13 §6): the
/// checkout's `.git` directory and every protected directory ([`vk_sandbox::gitexec`]), at
/// their place under the workspace mount. Only for providers that mount nested shares (Lima,
/// the fake); Tart links shares into place through the guest, which would write the link into
/// the host checkout — there host git hardening (the registered checkout) is the protection.
pub(crate) fn protect_mounts(
    spec: &mut VmSpec,
    kind: VmProviderKind,
    checkout: &Path,
    protected: &[PathBuf],
) {
    if kind == VmProviderKind::Tart {
        return;
    }
    let mut dirs = vec![checkout.join(".git")];
    dirs.extend(protected.iter().cloned());
    for d in dirs {
        let Ok(rel) = d.strip_prefix(checkout) else {
            continue;
        };
        if rel.as_os_str().is_empty() || !d.is_dir() || spec.mounts.iter().any(|m| m.host == d) {
            continue;
        }
        spec.mounts.push(VmMount {
            host: d.clone(),
            target: Path::new(vm::VM_WORKSPACE)
                .join(rel)
                .to_string_lossy()
                .into_owned(),
            read_only: true,
        });
    }
}

fn template_spec(c: &vk_sandbox::config::VmConfig) -> TemplateSpec {
    TemplateSpec {
        image: c.image.clone(),
        setup: c.setup.clone(),
        cpus: c.cpus,
        memory_mb: c.memory_mb(),
        disk_gb: c.disk_gb(),
    }
}

/// Run the configured setup commands in the template builder VM, in order; the first failure
/// ends it. (The builder has no checkout: setup is for toolchains and system packages.)
fn run_setup(cmds: Vec<String>) -> impl Fn(&dyn VmBackend, &str) -> vm::VmResult<()> {
    move |be, name| {
        for cmd in &cmds {
            let argv = be.exec_argv(name, &[c_sh(), "-c".into(), cmd.clone()], None, &[]);
            let o = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .stdin(std::process::Stdio::null())
                .output()?;
            if !o.status.success() {
                return Err(vm::VmError::Command {
                    what: format!("setup `{cmd}`"),
                    detail: String::from_utf8_lossy(&o.stderr).trim().to_string(),
                });
            }
        }
        Ok(())
    }
}

fn c_sh() -> String {
    "/bin/sh".into()
}

/// The box runner for a task's VM. `start` creates (or forks from the template) and boots the
/// VM; without it (restore after a server restart) the existing VM is only looked up.
pub async fn build_runner(
    server: &Arc<Server>,
    key: &str,
    task: Option<&str>,
    checkout: &Path,
    network: NetworkProfile,
    protected: &[PathBuf],
    start: bool,
) -> Result<VmBoxRunner, vk_proto::rpc::RpcError> {
    let c = gate(server)?;
    let be = backend(server)?;
    if be.kind() != VmProviderKind::Fake && network != NetworkProfile::Open {
        return Err(err(
            ErrorKind::Unsupported,
            format!(
                "network profile {} is not enforced for {} VMs yet (the host-to-VM link is not wired); use --network open to accept an unfiltered VM",
                network.as_str(),
                be.kind().as_str()
            ),
        ));
    }
    let name = vm_name(key);
    let m = VmManager::new(be.clone(), &server.paths.state);
    let (task_s, checkout_p, c2, name2, prot, kind) = (
        task.map(str::to_string),
        checkout.to_path_buf(),
        c.clone(),
        name.clone(),
        protected.to_vec(),
        be.kind(),
    );
    tokio::task::spawn_blocking(move || -> Result<(), vk_proto::rpc::RpcError> {
        let exists = m.backend().state(&name2).is_ok();
        if exists {
            // Restore, or a re-prepare: make sure it runs.
            if start && m.backend().state(&name2).map_err(vm_err)? != vm::VmState::Running {
                let _ = m.backend().start(&name2);
            }
            return Ok(());
        }
        if !start {
            return Err(err(ErrorKind::NotFound, format!("vm {name2} is gone")));
        }
        let mut spec = spec_of(&c2, &name2, Some(&checkout_p));
        protect_mounts(&mut spec, kind, &checkout_p, &prot);
        if c2.template {
            let ts = template_spec(&c2);
            let tpl = m
                .ensure_template(&ts, &run_setup(c2.setup.clone()))
                .map_err(vm_err)?;
            m.claim_from_template(&tpl.key, task_s.as_deref(), &spec)
                .map_err(vm_err)?;
        } else {
            m.create(task_s.as_deref(), &spec).map_err(vm_err)?;
        }
        Ok(())
    })
    .await
    .map_err(internal)??;
    Ok(VmBoxRunner {
        backend: be,
        vm: name,
        checkout: checkout.to_path_buf(),
        network,
        proxy: false,
        shell: c.shell,
    })
}

/// Destroy the VM of a task box (task teardown).
pub fn destroy_for(server: &Server, key: &str) {
    if let Ok(m) = manager(server) {
        let _ = m.destroy(&vm_name(key));
    }
}

pub async fn api(server: &Arc<Server>, _ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "vm.status" => Ok(status(server)),
        "vm.list" => list(server),
        "vm.create" => create(server, p).await,
        "vm.start" | "vm.stop" | "vm.suspend" | "vm.resume" | "vm.destroy" => {
            lifecycle(server, method, p).await
        }
        "vm.snapshot" => snapshot(server, p).await,
        "vm.snapshot.delete" => snapshot_delete(server, p).await,
        "vm.fork" => fork(server, p).await,
        "vm.transport" => transport(server, p),
        "vm.template.list" => template_list(server),
        "vm.template.build" => template_build(server).await,
        "vm.template.delete" => template_delete(server, p).await,
        _ => return None,
    })
}

fn status(server: &Server) -> Value {
    let c = vm_cfg(server);
    let avail = backend(server).and_then(|b| b.check().map_err(vm_err).map(|d| (b.kind(), d)));
    let (available, detail, provider) = match &avail {
        Ok((k, d)) => (true, d.clone(), k.as_str().to_string()),
        Err(e) => (false, e.message.clone(), c.provider.clone()),
    };
    json!({
        "enabled": c.enabled,
        "provider": provider,
        "available": available,
        "detail": detail,
        "config": {"image": c.image, "cpus": c.cpus, "memory_mb": c.memory_mb(), "disk_gb": c.disk_gb(), "template": c.template, "setup": c.setup, "transport": c.transport, "shell": c.shell},
        "providers": [
            {"provider": "fake", "status": "tested", "note": "directory-backed VMs; no kernel, no network"},
            {"provider": "lima", "status": "unverified", "note": "command lines generated and unit-tested; not run against a real VM (macOS vz)"},
            {"provider": "tart", "status": "unverified", "note": "command lines generated and unit-tested; not run against a real VM (Apple Silicon)"},
            {"provider": "firecracker", "status": "scaffold", "note": "configuration generation only; needs Linux with KVM"},
            {"provider": "cloud-hypervisor", "status": "scaffold", "note": "not implemented"},
        ],
    })
}

fn list(server: &Server) -> R {
    let m = manager(server)?;
    let reg = m.registry();
    let vms: Vec<Value> = reg
        .vms
        .values()
        .map(|v| json!({"name": v.name, "task": v.task, "template": v.template, "created_ms": v.created_ms, "state": m.backend().state(&v.name).map(|s| s.as_str().to_string()).unwrap_or_else(|_| "missing".into())}))
        .collect();
    Ok(
        json!({"vms": vms, "snapshots": reg.snapshots.values().map(|s| json!({"id": s.snap.id, "vm": s.snap.vm, "label": s.snap.label, "memory": s.snap.memory, "task": s.task, "template": s.template, "created_ms": s.snap.created_ms})).collect::<Vec<_>>()}),
    )
}

async fn create(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let m = manager(server)?;
    let name = s(p, "name")
        .map(str::to_string)
        .unwrap_or_else(|| format!("vk-{}", &crate::core::ulid().to_lowercase()[18..]));
    let task = s(p, "task").map(str::to_string);
    let checkout = s(p, "checkout").map(PathBuf::from);
    let use_template = b(p, "template").unwrap_or(c.template);
    let (c2, name2) = (c.clone(), name.clone());
    let (info, fork_ms) = tokio::task::spawn_blocking(move || {
        let spec = spec_of(&c2, &name2, checkout.as_deref());
        if use_template {
            let ts = template_spec(&c2);
            let tpl = m.ensure_template(&ts, &run_setup(c2.setup.clone()))?;
            let (i, ms) = m.claim_from_template(&tpl.key, task.as_deref(), &spec)?;
            Ok((i, Some(ms)))
        } else {
            Ok((m.create(task.as_deref(), &spec)?, None))
        }
    })
    .await
    .map_err(internal)?
    .map_err(vm_err)?;
    orch::emit(
        server,
        "vm.created",
        json!({"vm": info.name}),
        json!({"provider": info.provider.as_str(), "template": use_template, "fork_ms": fork_ms}),
    );
    Ok(json!({"vm": info, "fork_ms": fork_ms}))
}

async fn lifecycle(server: &Arc<Server>, method: &str, p: &Value) -> R {
    gate(server)?;
    let name = req(p, "vm")?.to_string();
    let m = manager(server)?;
    let op = method.trim_start_matches("vm.").to_string();
    let (op2, name2) = (op.clone(), name.clone());
    let state = tokio::task::spawn_blocking(move || -> vm::VmResult<Option<vm::VmState>> {
        match op2.as_str() {
            "start" => m.backend().start(&name2)?,
            "stop" => m.stop(&name2)?,
            "suspend" => m.suspend(&name2)?,
            "resume" => m.resume(&name2)?,
            "destroy" => {
                m.destroy(&name2)?;
                return Ok(None);
            }
            _ => unreachable!(),
        }
        Ok(m.backend().state(&name2).ok())
    })
    .await
    .map_err(internal)?
    .map_err(vm_err)?;
    let kind = match op.as_str() {
        "start" => "vm.started",
        "stop" => "vm.stopped",
        "suspend" => "vm.suspended",
        "resume" => "vm.resumed",
        _ => "vm.destroyed",
    };
    orch::emit(
        server,
        kind,
        json!({"vm": name}),
        json!({"state": state.map(|s| s.as_str())}),
    );
    Ok(json!({"vm": name, "state": state.map(|s| s.as_str())}))
}

async fn snapshot(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let name = req(p, "vm")?.to_string();
    let label = s(p, "label")
        .map(str::to_string)
        .unwrap_or_else(|| format!("snap-{}", vk_store::now_ms() / 1000));
    let m = manager(server)?;
    let (n2, l2) = (name.clone(), label.clone());
    let snap = tokio::task::spawn_blocking(move || m.snapshot(&n2, &l2))
        .await
        .map_err(internal)?
        .map_err(vm_err)?;
    orch::emit(
        server,
        "vm.snapshot_created",
        json!({"vm": name}),
        json!({"snapshot": snap.id, "memory": snap.memory}),
    );
    Ok(json!({"snapshot": snap}))
}

async fn snapshot_delete(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let id = req(p, "snapshot")?.to_string();
    let m = manager(server)?;
    let (i2, force) = (id.clone(), b(p, "force").unwrap_or(false));
    tokio::task::spawn_blocking(move || m.delete_snapshot(&i2, force))
        .await
        .map_err(internal)?
        .map_err(vm_err)?;
    Ok(json!({"deleted": id}))
}

async fn fork(server: &Arc<Server>, p: &Value) -> R {
    let c = gate(server)?;
    let snap = req(p, "snapshot")?.to_string();
    let count = u(p, "count").unwrap_or(1).clamp(1, 16) as usize;
    let prefix = s(p, "prefix")
        .map(str::to_string)
        .unwrap_or_else(|| format!("vk-fork-{}", &crate::core::ulid().to_lowercase()[20..]));
    let tasks: Vec<Option<String>> = (0..count)
        .map(|i| {
            p.get("tasks")
                .and_then(Value::as_array)
                .and_then(|a| a.get(i))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .collect();
    let checkouts: Vec<Option<PathBuf>> = (0..count)
        .map(|i| {
            p.get("checkouts")
                .and_then(Value::as_array)
                .and_then(|a| a.get(i))
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
        .collect();
    let m = manager(server)?;
    let specs: Vec<(Option<String>, VmSpec)> = (0..count)
        .map(|i| {
            (
                tasks[i].clone(),
                spec_of(&c, &format!("{prefix}-{}", i + 1), checkouts[i].as_deref()),
            )
        })
        .collect();
    let s2 = snap.clone();
    let started = std::time::Instant::now();
    let vms = tokio::task::spawn_blocking(move || m.fork_many(&s2, &specs))
        .await
        .map_err(internal)?
        .map_err(vm_err)?;
    let ms = started.elapsed().as_millis() as u64;
    orch::emit(
        server,
        "vm.forked",
        json!({"snapshot": snap}),
        json!({"vms": vms.iter().map(|v| v.name.clone()).collect::<Vec<_>>(), "ms": ms}),
    );
    Ok(json!({"vms": vms, "ms": ms}))
}

fn transport(server: &Server, p: &Value) -> R {
    let name = req(p, "vm")?;
    let be = backend(server)?;
    let want = vm_transport::parse(&vm_cfg(server).transport).map_err(invalid)?;
    let offered = be.transports(name);
    let chain = vm_transport::plan(&offered, want).map_err(|e| err(ErrorKind::Unsupported, e))?;
    Ok(json!({
        "vm": name,
        "offered": offered.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        "chain": chain.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
        "configured": vm_cfg(server).transport,
        "link": "the in-VM bridge (`vibeke sandbox bridge`) is carried over the first transport that connects; only `exec` has a dialer, and the egress/broker link over it is not wired for real providers",
    }))
}

fn template_list(server: &Server) -> R {
    let m = manager(server)?;
    Ok(json!({"templates": m.registry().templates.values().collect::<Vec<_>>()}))
}

async fn template_build(server: &Arc<Server>) -> R {
    let c = gate(server)?;
    let m = manager(server)?;
    let ts = template_spec(&c);
    let setup = c.setup.clone();
    let tpl = tokio::task::spawn_blocking(move || m.ensure_template(&ts, &run_setup(setup)))
        .await
        .map_err(internal)?
        .map_err(vm_err)?;
    orch::emit(
        server,
        "vm.template_built",
        json!({"template": tpl.key}),
        json!({"snapshot": tpl.snapshot.id, "setup_digest": tpl.setup_digest}),
    );
    Ok(json!({"template": tpl}))
}

async fn template_delete(server: &Arc<Server>, p: &Value) -> R {
    gate(server)?;
    let key = req(p, "key")?.to_string();
    let m = manager(server)?;
    let k2 = key.clone();
    tokio::task::spawn_blocking(move || m.delete_template(&k2))
        .await
        .map_err(internal)?
        .map_err(vm_err)?;
    Ok(json!({"deleted": key}))
}

/// A fake backend rooted under `dir` (what `provider = "fake"` builds), for tests.
pub fn fake_backend(dir: &Path) -> Arc<dyn VmBackend> {
    Arc::new(FakeVmBackend::new(dir))
}
