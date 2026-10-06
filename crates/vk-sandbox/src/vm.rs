//! The `vm` level (13 §2.1, §9, §13 M4): provider backends, template snapshots, fork for
//! best-of-N, and the [`VmBoxRunner`] that wraps pane commands for a running VM.
//!
//! What is real and what is scaffolding:
//! - The [`VmBackend`] trait is the whole provider surface (create, start, stop, suspend,
//!   resume, destroy, snapshot, fork, exec, transports).
//! - [`FakeVmBackend`] implements it on plain directories and is what the tests and
//!   `isolation.vm.provider = "fake"` use. It has no kernel and no network; a "VM" is a disk
//!   directory commands run in.
//! - [`crate::vm_backends::LimaBackend`] and [`crate::vm_backends::TartBackend`] generate the
//!   real `limactl` / `tart` command lines and are unit-tested against a recording command
//!   runner. They have **not** been run against a real VM: that needs Apple Silicon (Tart,
//!   Lima `vz`) and is listed in the gap audit under "needs the user".
//! - [`FirecrackerBackend`] only produces the machine/snapshot configuration JSON (13 §2.1 for
//!   Linux with KVM) and reports itself unavailable everywhere else.
//!
//! Everything is off unless `[isolation.vm] enabled = true`; `provider = "auto"` picks Tart or
//! Lima on macOS and nothing elsewhere (Firecracker needs explicit setup).

use crate::net::NetworkProfile;
use crate::runner::{Mount, PreparedSpawn, Runner, RunnerError, SpawnRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use vk_proto::model::IsolationLevel;

/// Where the task checkout appears inside every VM.
pub const VM_WORKSPACE: &str = "/workspace";
/// The in-VM `vibeke` binary (same convention as containers).
pub const VM_BIN: &str = "/vibeke/bin/vibeke";
/// Where the in-VM link end listens for the egress proxy (same as containers).
pub const VM_PROXY_PORT: u16 = 3128;

#[derive(Debug, thiserror::Error)]
pub enum VmError {
    #[error("vm {0} not found")]
    NotFound(String),
    #[error("vm {name}: {msg}")]
    State { name: String, msg: String },
    #[error("{0}")]
    Unavailable(String),
    #[error("{what}: {detail}")]
    Command { what: String, detail: String },
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Invalid(String),
}

pub type VmResult<T> = std::result::Result<T, VmError>;

impl From<VmError> for RunnerError {
    fn from(e: VmError) -> RunnerError {
        match e {
            VmError::Unavailable(reason) => RunnerError::Unavailable {
                level: "vm",
                reason,
            },
            other => RunnerError::Unsupported(other.to_string()),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmProviderKind {
    Lima,
    Tart,
    Firecracker,
    CloudHypervisor,
    Fake,
}

impl VmProviderKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VmProviderKind::Lima => "lima",
            VmProviderKind::Tart => "tart",
            VmProviderKind::Firecracker => "firecracker",
            VmProviderKind::CloudHypervisor => "cloud-hypervisor",
            VmProviderKind::Fake => "fake",
        }
    }
    pub fn parse(s: &str) -> Option<VmProviderKind> {
        Some(match s {
            "lima" => VmProviderKind::Lima,
            "tart" => VmProviderKind::Tart,
            "firecracker" => VmProviderKind::Firecracker,
            "cloud-hypervisor" | "cloud_hypervisor" => VmProviderKind::CloudHypervisor,
            "fake" => VmProviderKind::Fake,
            _ => return None,
        })
    }
    /// Can fork from a snapshot including memory (sub-second start, 13 §9)?
    pub fn memory_snapshots(self) -> bool {
        matches!(
            self,
            VmProviderKind::Firecracker | VmProviderKind::CloudHypervisor
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VmState {
    Created,
    Running,
    Suspended,
    Stopped,
}

impl VmState {
    pub fn as_str(self) -> &'static str {
        match self {
            VmState::Created => "created",
            VmState::Running => "running",
            VmState::Suspended => "suspended",
            VmState::Stopped => "stopped",
        }
    }
}

/// What a VM is created with.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmSpec {
    pub name: String,
    pub image: Option<String>,
    pub cpus: u32,
    pub memory_mb: u64,
    pub disk_gb: u64,
    /// Host directories shared into the VM (the task checkout at [`VM_WORKSPACE`], the inbox).
    pub mounts: Vec<VmMount>,
    pub network: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmMount {
    pub host: PathBuf,
    pub target: String,
    pub read_only: bool,
}

impl VmSpec {
    pub fn new(name: &str) -> VmSpec {
        VmSpec {
            name: name.to_string(),
            image: None,
            cpus: 4,
            memory_mb: 8192,
            disk_gb: 30,
            mounts: vec![],
            network: NetworkProfile::Dev.as_str().to_string(),
        }
    }
}

/// VM names become directory and instance names.
pub fn valid_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 48
        && n.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !n.starts_with('-')
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmInfo {
    pub name: String,
    pub provider: VmProviderKind,
    pub state: VmState,
    pub cpus: u32,
    pub memory_mb: u64,
    pub created_ms: i64,
    /// The snapshot this VM was forked from.
    #[serde(default)]
    pub forked_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotInfo {
    /// `<vm>/<label>`.
    pub id: String,
    pub vm: String,
    pub label: String,
    /// The snapshot includes memory (a fork resumes mid-execution instead of booting).
    pub memory: bool,
    pub created_ms: i64,
    pub provider: VmProviderKind,
}

/// Host-to-VM link carrying the in-VM `vibeke sandbox bridge` (egress, brokers, previews).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum VmTransportKind {
    Vsock,
    VirtioSerial,
    /// The provider's own exec channel (`limactl shell`, `tart exec`).
    Exec,
    Ssh,
}

impl VmTransportKind {
    pub fn as_str(self) -> &'static str {
        match self {
            VmTransportKind::Vsock => "vsock",
            VmTransportKind::VirtioSerial => "virtio-serial",
            VmTransportKind::Exec => "exec",
            VmTransportKind::Ssh => "ssh",
        }
    }
}

pub trait VmBackend: Send + Sync {
    fn kind(&self) -> VmProviderKind;
    /// `Ok(detail)` when the provider can run VMs here (`vibeke doctor`).
    fn check(&self) -> VmResult<String>;
    /// Create (not start) a VM.
    fn create(&self, spec: &VmSpec) -> VmResult<VmInfo>;
    fn start(&self, name: &str) -> VmResult<()>;
    fn stop(&self, name: &str) -> VmResult<()>;
    /// Save state and stop (13 §11 `task park`).
    fn suspend(&self, name: &str) -> VmResult<()>;
    fn resume(&self, name: &str) -> VmResult<()>;
    fn destroy(&self, name: &str) -> VmResult<()>;
    fn state(&self, name: &str) -> VmResult<VmState>;
    fn snapshot(&self, name: &str, label: &str) -> VmResult<SnapshotInfo>;
    /// A new, not yet started VM from `snap`, with `spec`'s mounts and limits.
    fn fork(&self, snap: &SnapshotInfo, spec: &VmSpec) -> VmResult<VmInfo>;
    fn delete_snapshot(&self, snap: &SnapshotInfo) -> VmResult<()>;
    /// Does the provider still have this snapshot? (A template whose snapshot was deleted
    /// behind Vibeke's back is rebuilt.) Providers that cannot tell say yes.
    fn snapshot_exists(&self, _snap: &SnapshotInfo) -> bool {
        true
    }
    /// The host-side command that runs `argv` inside the VM.
    fn exec_argv(
        &self,
        name: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Vec<String>;
    /// Link transports the VM offers, best first.
    fn transports(&self, name: &str) -> Vec<VmTransportKind>;
}

// ---- the fake backend -----------------------------------------------------------------------

/// Directory-backed VMs for tests and `provider = "fake"`: `<root>/vms/<name>/{vm.json,disk/}`
/// and `<root>/snapshots/<vm>/<label>/{snap.json,disk/}`. Mounts are symlinks inside `disk/`
/// (and are never copied into snapshots). Commands run with their cwd inside `disk/`.
pub struct FakeVmBackend {
    root: PathBuf,
    /// Behave like a provider with memory snapshots (Firecracker): snapshots keep `memory`.
    pub memory_snapshots: bool,
    log: Mutex<Vec<String>>,
    /// (op, successful calls to let through first, message)
    fail: Mutex<Vec<(String, u32, String)>>,
}

#[derive(Serialize, Deserialize)]
struct FakeMeta {
    info: VmInfo,
    spec: VmSpec,
}

fn copy_tree(from: &Path, to: &Path, skip: &[PathBuf]) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let src = e.path();
        if skip.iter().any(|s| s == &src) {
            continue;
        }
        let dst = to.join(e.file_name());
        let ft = e.file_type()?;
        if ft.is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(&src)?, &dst)?;
        } else if ft.is_dir() {
            copy_tree(&src, &dst, skip)?;
        } else {
            std::fs::copy(&src, &dst)?;
        }
    }
    Ok(())
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl FakeVmBackend {
    pub fn new(root: &Path) -> FakeVmBackend {
        FakeVmBackend {
            root: root.to_path_buf(),
            memory_snapshots: false,
            log: Mutex::new(vec![]),
            fail: Mutex::new(vec![]),
        }
    }
    /// Every backend call so far, as `op:arg` strings.
    pub fn calls(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
    /// Make the next call of `op` fail with `msg` (once).
    pub fn fail_next(&self, op: &str, msg: &str) {
        self.fail_nth(op, 0, msg);
    }
    /// Let `skip` calls of `op` succeed, then fail the next one (once).
    pub fn fail_nth(&self, op: &str, skip: u32, msg: &str) {
        self.fail
            .lock()
            .unwrap()
            .push((op.to_string(), skip, msg.to_string()));
    }
    fn enter(&self, op: &str, arg: &str) -> VmResult<()> {
        self.log.lock().unwrap().push(format!("{op}:{arg}"));
        let mut f = self.fail.lock().unwrap();
        if let Some(i) = f.iter().position(|(o, _, _)| o == op) {
            if f[i].1 > 0 {
                f[i].1 -= 1;
                return Ok(());
            }
            let (_, _, m) = f.remove(i);
            return Err(VmError::Command {
                what: op.to_string(),
                detail: m,
            });
        }
        Ok(())
    }
    fn dir(&self, name: &str) -> PathBuf {
        self.root.join("vms").join(name)
    }
    /// The directory standing in for the VM's filesystem.
    pub fn disk(&self, name: &str) -> PathBuf {
        self.dir(name).join("disk")
    }
    fn snap_dir(&self, snap: &SnapshotInfo) -> PathBuf {
        self.root.join("snapshots").join(&snap.vm).join(&snap.label)
    }
    fn read(&self, name: &str) -> VmResult<FakeMeta> {
        let p = self.dir(name).join("vm.json");
        let b = std::fs::read(&p).map_err(|_| VmError::NotFound(name.to_string()))?;
        serde_json::from_slice(&b).map_err(|e| VmError::Invalid(e.to_string()))
    }
    fn write(&self, name: &str, m: &FakeMeta) -> VmResult<()> {
        let p = self.dir(name).join("vm.json");
        std::fs::write(
            p,
            serde_json::to_vec_pretty(m).map_err(|e| VmError::Invalid(e.to_string()))?,
        )?;
        Ok(())
    }
    fn set_state(&self, name: &str, from: &[VmState], to: VmState) -> VmResult<()> {
        let mut m = self.read(name)?;
        if !from.contains(&m.info.state) {
            return Err(VmError::State {
                name: name.to_string(),
                msg: format!(
                    "is {}; expected {}",
                    m.info.state.as_str(),
                    from.iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(" or ")
                ),
            });
        }
        m.info.state = to;
        self.write(name, &m)
    }
    fn link_mounts(&self, disk: &Path, spec: &VmSpec) -> VmResult<()> {
        for m in &spec.mounts {
            let rel = m.target.trim_start_matches('/');
            if rel.is_empty() || rel.split('/').any(|c| c == "..") {
                return Err(VmError::Invalid(format!("bad mount target {}", m.target)));
            }
            let at = disk.join(rel);
            if let Some(p) = at.parent() {
                std::fs::create_dir_all(p)?;
            }
            if at.symlink_metadata().is_ok() {
                continue;
            }
            std::os::unix::fs::symlink(&m.host, &at)?;
        }
        Ok(())
    }
    fn mount_paths(&self, disk: &Path, spec: &VmSpec) -> Vec<PathBuf> {
        spec.mounts
            .iter()
            .map(|m| disk.join(m.target.trim_start_matches('/')))
            .collect()
    }
}

impl VmBackend for FakeVmBackend {
    fn kind(&self) -> VmProviderKind {
        VmProviderKind::Fake
    }
    fn check(&self) -> VmResult<String> {
        Ok(format!(
            "fake backend at {} (no kernel, no network; for tests)",
            self.root.display()
        ))
    }
    fn create(&self, spec: &VmSpec) -> VmResult<VmInfo> {
        self.enter("create", &spec.name)?;
        if !valid_name(&spec.name) {
            return Err(VmError::Invalid(format!(
                "`{}` is not a valid vm name",
                spec.name
            )));
        }
        if self.dir(&spec.name).exists() {
            return Err(VmError::State {
                name: spec.name.clone(),
                msg: "already exists".into(),
            });
        }
        std::fs::create_dir_all(self.disk(&spec.name))?;
        self.link_mounts(&self.disk(&spec.name), spec)?;
        let info = VmInfo {
            name: spec.name.clone(),
            provider: VmProviderKind::Fake,
            state: VmState::Created,
            cpus: spec.cpus,
            memory_mb: spec.memory_mb,
            created_ms: now_ms(),
            forked_from: None,
        };
        self.write(
            &spec.name,
            &FakeMeta {
                info: info.clone(),
                spec: spec.clone(),
            },
        )?;
        Ok(info)
    }
    fn start(&self, name: &str) -> VmResult<()> {
        self.enter("start", name)?;
        self.set_state(
            name,
            &[VmState::Created, VmState::Stopped],
            VmState::Running,
        )
    }
    fn stop(&self, name: &str) -> VmResult<()> {
        self.enter("stop", name)?;
        self.set_state(
            name,
            &[VmState::Running, VmState::Suspended, VmState::Created],
            VmState::Stopped,
        )
    }
    fn suspend(&self, name: &str) -> VmResult<()> {
        self.enter("suspend", name)?;
        self.set_state(name, &[VmState::Running], VmState::Suspended)
    }
    fn resume(&self, name: &str) -> VmResult<()> {
        self.enter("resume", name)?;
        self.set_state(name, &[VmState::Suspended], VmState::Running)
    }
    fn destroy(&self, name: &str) -> VmResult<()> {
        self.enter("destroy", name)?;
        let d = self.dir(name);
        if !d.exists() {
            return Err(VmError::NotFound(name.to_string()));
        }
        // remove_dir_all unlinks mount symlinks without following them.
        std::fs::remove_dir_all(d)?;
        Ok(())
    }
    fn state(&self, name: &str) -> VmResult<VmState> {
        Ok(self.read(name)?.info.state)
    }
    fn snapshot(&self, name: &str, label: &str) -> VmResult<SnapshotInfo> {
        self.enter("snapshot", &format!("{name}/{label}"))?;
        if label.is_empty()
            || !label
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(VmError::Invalid(format!(
                "`{label}` is not a valid snapshot label"
            )));
        }
        let m = self.read(name)?;
        let snap = SnapshotInfo {
            id: format!("{name}/{label}"),
            vm: name.to_string(),
            label: label.to_string(),
            memory: self.memory_snapshots && m.info.state == VmState::Running,
            created_ms: now_ms(),
            provider: VmProviderKind::Fake,
        };
        let d = self.snap_dir(&snap);
        if d.exists() {
            return Err(VmError::State {
                name: name.to_string(),
                msg: format!("snapshot {label} already exists"),
            });
        }
        let disk = self.disk(name);
        copy_tree(&disk, &d.join("disk"), &self.mount_paths(&disk, &m.spec))?;
        std::fs::write(
            d.join("snap.json"),
            serde_json::to_vec_pretty(&snap).map_err(|e| VmError::Invalid(e.to_string()))?,
        )?;
        Ok(snap)
    }
    fn fork(&self, snap: &SnapshotInfo, spec: &VmSpec) -> VmResult<VmInfo> {
        self.enter("fork", &format!("{}->{}", snap.id, spec.name))?;
        if !valid_name(&spec.name) {
            return Err(VmError::Invalid(format!(
                "`{}` is not a valid vm name",
                spec.name
            )));
        }
        let from = self.snap_dir(snap).join("disk");
        if !from.exists() {
            return Err(VmError::NotFound(snap.id.clone()));
        }
        if self.dir(&spec.name).exists() {
            return Err(VmError::State {
                name: spec.name.clone(),
                msg: "already exists".into(),
            });
        }
        let disk = self.disk(&spec.name);
        copy_tree(&from, &disk, &[])?;
        self.link_mounts(&disk, spec)?;
        let info = VmInfo {
            name: spec.name.clone(),
            provider: VmProviderKind::Fake,
            // A memory snapshot resumes where it was; a disk snapshot boots.
            state: VmState::Created,
            cpus: spec.cpus,
            memory_mb: spec.memory_mb,
            created_ms: now_ms(),
            forked_from: Some(snap.id.clone()),
        };
        self.write(
            &spec.name,
            &FakeMeta {
                info: info.clone(),
                spec: spec.clone(),
            },
        )?;
        Ok(info)
    }
    fn delete_snapshot(&self, snap: &SnapshotInfo) -> VmResult<()> {
        self.enter("delete_snapshot", &snap.id)?;
        let d = self.snap_dir(snap);
        if !d.exists() {
            return Err(VmError::NotFound(snap.id.clone()));
        }
        std::fs::remove_dir_all(d)?;
        Ok(())
    }
    fn snapshot_exists(&self, snap: &SnapshotInfo) -> bool {
        self.snap_dir(snap).join("disk").exists()
    }
    fn exec_argv(
        &self,
        name: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Vec<String> {
        let dir = match cwd {
            Some(c) => self.disk(name).join(c.trim_start_matches('/')),
            None => self.disk(name),
        };
        let mut v: Vec<String> = vec![
            "/bin/sh".into(),
            "-c".into(),
            "cd \"$1\" && shift && exec \"$@\"".into(),
            "vibeke-vm".into(),
            dir.to_string_lossy().into_owned(),
            "/usr/bin/env".into(),
            format!("VIBEKE_VM={name}"),
        ];
        v.extend(env.iter().map(|(k, val)| format!("{k}={val}")));
        v.extend(argv.iter().cloned());
        v
    }
    fn transports(&self, _name: &str) -> Vec<VmTransportKind> {
        vec![VmTransportKind::Exec]
    }
}

// ---- Firecracker scaffolding ----------------------------------------------------------------

/// Firecracker (Linux with KVM): configuration only. Lifecycle calls report the provider as
/// unavailable; the generated JSON is what a real implementation would PUT to the API socket.
pub struct FirecrackerBackend {
    pub kernel: PathBuf,
    pub rootfs: PathBuf,
    pub bin: PathBuf,
}

impl FirecrackerBackend {
    pub fn machine_config(&self, spec: &VmSpec, vsock_cid: u32, uds: &Path) -> serde_json::Value {
        serde_json::json!({
            "boot-source": {
                "kernel_image_path": self.kernel,
                "boot_args": "console=ttyS0 reboot=k panic=1 pci=off",
            },
            "drives": [{
                "drive_id": "rootfs",
                "path_on_host": self.rootfs,
                "is_root_device": true,
                "is_read_only": false,
            }],
            "machine-config": {"vcpu_count": spec.cpus, "mem_size_mib": spec.memory_mb, "track_dirty_pages": true},
            "vsock": {"guest_cid": vsock_cid, "uds_path": uds},
        })
    }
    /// `PUT /snapshot/create` body: a full snapshot (memory + state).
    pub fn snapshot_create_body(&self, dir: &Path) -> serde_json::Value {
        serde_json::json!({
            "snapshot_type": "Full",
            "snapshot_path": dir.join("vm.state"),
            "mem_file_path": dir.join("vm.mem"),
        })
    }
    /// `PUT /snapshot/load` body: fork a VM from a snapshot, resuming it immediately.
    pub fn snapshot_load_body(&self, dir: &Path) -> serde_json::Value {
        serde_json::json!({
            "snapshot_path": dir.join("vm.state"),
            "mem_backend": {"backend_path": dir.join("vm.mem"), "backend_type": "File"},
            "enable_diff_snapshots": true,
            "resume_vm": true,
        })
    }
    pub fn check(&self) -> VmResult<String> {
        if !cfg!(target_os = "linux") {
            return Err(VmError::Unavailable(
                "Firecracker needs a Linux host with KVM".into(),
            ));
        }
        if !Path::new("/dev/kvm").exists() {
            return Err(VmError::Unavailable(
                "/dev/kvm is not available (no KVM access)".into(),
            ));
        }
        if !self.bin.is_file() {
            return Err(VmError::Unavailable(format!(
                "{} not found",
                self.bin.display()
            )));
        }
        Err(VmError::Unavailable(
            "the Firecracker lifecycle is scaffolding only: configuration is generated but no VM is started; use lima or tart".into(),
        ))
    }
}

// ---- manager: templates, claims, forks -------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VmRecord {
    pub name: String,
    pub task: Option<String>,
    pub template: Option<String>,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TemplateRecord {
    pub key: String,
    pub snapshot: SnapshotInfo,
    pub image: Option<String>,
    pub setup_digest: String,
    pub created_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapRecord {
    pub snap: SnapshotInfo,
    pub task: Option<String>,
    pub template: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Registry {
    pub vms: BTreeMap<String, VmRecord>,
    pub snapshots: BTreeMap<String, SnapRecord>,
    pub templates: BTreeMap<String, TemplateRecord>,
}

/// What defines a template (13 §9): image, setup and size. Same inputs, same template.
#[derive(Debug, Clone, PartialEq)]
pub struct TemplateSpec {
    pub image: Option<String>,
    pub setup: Vec<String>,
    pub cpus: u32,
    pub memory_mb: u64,
    pub disk_gb: u64,
}

impl TemplateSpec {
    pub fn key(&self) -> String {
        let mut h = blake3::Hasher::new();
        for p in [
            self.image.as_deref().unwrap_or(""),
            &self.setup.join("\n"),
            &self.cpus.to_string(),
            &self.memory_mb.to_string(),
            &self.disk_gb.to_string(),
        ] {
            h.update(p.as_bytes());
            h.update(b"\0");
        }
        h.finalize().to_hex()[..12].to_string()
    }
    pub fn setup_digest(&self) -> String {
        blake3::hash(self.setup.join("\n").as_bytes()).to_hex()[..12].to_string()
    }
}

fn reg_lock() -> &'static Mutex<()> {
    static L: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
    L.get_or_init(Mutex::default)
}

/// Tracks the VMs, snapshots and templates Vibeke created (`<dir>/vm-registry.json`) on top of
/// a backend. The backend is the source of truth for what exists; the registry says which task
/// owns what and which snapshot is which template.
pub struct VmManager {
    backend: Arc<dyn VmBackend>,
    file: PathBuf,
}

impl VmManager {
    pub fn new(backend: Arc<dyn VmBackend>, dir: &Path) -> VmManager {
        VmManager {
            backend,
            file: dir.join("vm-registry.json"),
        }
    }

    pub fn backend(&self) -> &Arc<dyn VmBackend> {
        &self.backend
    }

    pub fn registry(&self) -> Registry {
        let _g = reg_lock().lock().unwrap();
        self.load()
    }

    fn load(&self) -> Registry {
        std::fs::read(&self.file)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn update<T>(&self, f: impl FnOnce(&mut Registry) -> T) -> VmResult<T> {
        let _g = reg_lock().lock().unwrap();
        let mut r = self.load();
        let out = f(&mut r);
        if let Some(p) = self.file.parent() {
            std::fs::create_dir_all(p)?;
        }
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(
            &tmp,
            serde_json::to_vec_pretty(&r).map_err(|e| VmError::Invalid(e.to_string()))?,
        )?;
        std::fs::rename(tmp, &self.file)?;
        Ok(out)
    }

    /// Create and start a VM for `task`.
    pub fn create(&self, task: Option<&str>, spec: &VmSpec) -> VmResult<VmInfo> {
        let info = self.backend.create(spec)?;
        if let Err(e) = self.backend.start(&spec.name) {
            let _ = self.backend.destroy(&spec.name);
            return Err(e);
        }
        let name = spec.name.clone();
        let t = task.map(str::to_string);
        self.update(|r| {
            r.vms.insert(
                name.clone(),
                VmRecord {
                    name,
                    task: t,
                    template: None,
                    created_ms: now_ms(),
                },
            );
        })?;
        Ok(VmInfo {
            state: VmState::Running,
            ..info
        })
    }

    /// The template for `ts`, building it when missing: a builder VM runs `setup` through
    /// `run_setup(backend, builder_name)`, is snapshotted and destroyed. An existing template
    /// is reused untouched (setup cached per template, 13 §9).
    pub fn ensure_template(
        &self,
        ts: &TemplateSpec,
        run_setup: &dyn Fn(&dyn VmBackend, &str) -> VmResult<()>,
    ) -> VmResult<TemplateRecord> {
        let key = ts.key();
        if let Some(t) = self.registry().templates.get(&key)
            && self.snapshot_exists(&t.snapshot)
        {
            return Ok(t.clone());
        }
        let builder = format!("vk-tpl-{key}");
        let mut spec = VmSpec::new(&builder);
        spec.image = ts.image.clone();
        spec.cpus = ts.cpus;
        spec.memory_mb = ts.memory_mb;
        spec.disk_gb = ts.disk_gb;
        let _ = self.backend.destroy(&builder);
        self.backend.create(&spec)?;
        let built = (|| -> VmResult<SnapshotInfo> {
            self.backend.start(&builder)?;
            run_setup(self.backend.as_ref(), &builder)?;
            self.backend.snapshot(&builder, &format!("tpl-{key}"))
        })();
        let _ = self.backend.destroy(&builder);
        let snap = built?;
        let rec = TemplateRecord {
            key: key.clone(),
            snapshot: snap.clone(),
            image: ts.image.clone(),
            setup_digest: ts.setup_digest(),
            created_ms: now_ms(),
        };
        let r2 = rec.clone();
        self.update(|r| {
            r.snapshots.insert(
                snap.id.clone(),
                SnapRecord {
                    snap,
                    task: None,
                    template: Some(key.clone()),
                },
            );
            r.templates.insert(key, r2);
        })?;
        Ok(rec)
    }

    fn snapshot_exists(&self, s: &SnapshotInfo) -> bool {
        self.registry().snapshots.contains_key(&s.id) && self.backend.snapshot_exists(s)
    }

    /// Fork a started VM for `task` from the template; returns it and how long the fork and
    /// start took (ms).
    pub fn claim_from_template(
        &self,
        key: &str,
        task: Option<&str>,
        spec: &VmSpec,
    ) -> VmResult<(VmInfo, u64)> {
        let t = self
            .registry()
            .templates
            .get(key)
            .cloned()
            .ok_or_else(|| VmError::NotFound(format!("template {key}")))?;
        let started = Instant::now();
        let info = self.fork_one(&t.snapshot, task, spec, Some(key))?;
        Ok((info, started.elapsed().as_millis() as u64))
    }

    fn fork_one(
        &self,
        snap: &SnapshotInfo,
        task: Option<&str>,
        spec: &VmSpec,
        template: Option<&str>,
    ) -> VmResult<VmInfo> {
        let info = self.backend.fork(snap, spec)?;
        // A memory snapshot is already running when forked; a disk snapshot boots.
        if !snap.memory
            && let Err(e) = self.backend.start(&spec.name)
        {
            let _ = self.backend.destroy(&spec.name);
            return Err(e);
        }
        let name = spec.name.clone();
        let (t, tpl) = (task.map(str::to_string), template.map(str::to_string));
        self.update(|r| {
            r.vms.insert(
                name.clone(),
                VmRecord {
                    name,
                    task: t,
                    template: tpl,
                    created_ms: now_ms(),
                },
            );
        })?;
        Ok(VmInfo {
            state: VmState::Running,
            ..info
        })
    }

    /// Snapshot a VM under `label` (best-of-N base, "branch this agent at turn N").
    pub fn snapshot(&self, vm: &str, label: &str) -> VmResult<SnapshotInfo> {
        let task = self.registry().vms.get(vm).and_then(|v| v.task.clone());
        let snap = self.backend.snapshot(vm, label)?;
        let s2 = snap.clone();
        self.update(|r| {
            r.snapshots.insert(
                s2.id.clone(),
                SnapRecord {
                    snap: s2,
                    task,
                    template: None,
                },
            );
        })?;
        Ok(snap)
    }

    /// Fork one VM per spec from the same snapshot. All or nothing: if one fork fails the
    /// ones already made are destroyed.
    pub fn fork_many(
        &self,
        snapshot_id: &str,
        specs: &[(Option<String>, VmSpec)],
    ) -> VmResult<Vec<VmInfo>> {
        let snap = self
            .registry()
            .snapshots
            .get(snapshot_id)
            .map(|s| s.snap.clone())
            .ok_or_else(|| VmError::NotFound(format!("snapshot {snapshot_id}")))?;
        let mut made: Vec<VmInfo> = vec![];
        for (task, spec) in specs {
            match self.fork_one(&snap, task.as_deref(), spec, None) {
                Ok(i) => made.push(i),
                Err(e) => {
                    for m in &made {
                        let _ = self.destroy(&m.name);
                    }
                    return Err(e);
                }
            }
        }
        Ok(made)
    }

    pub fn suspend(&self, vm: &str) -> VmResult<()> {
        self.backend.suspend(vm)
    }
    pub fn resume(&self, vm: &str) -> VmResult<()> {
        self.backend.resume(vm)
    }
    pub fn stop(&self, vm: &str) -> VmResult<()> {
        self.backend.stop(vm)
    }

    pub fn destroy(&self, vm: &str) -> VmResult<()> {
        let r = self.backend.destroy(vm);
        let n = vm.to_string();
        self.update(|reg| {
            reg.vms.remove(&n);
        })?;
        match r {
            Err(VmError::NotFound(_)) => Ok(()),
            other => other,
        }
    }

    /// Delete a snapshot that is not a template's (or any, with `force`).
    pub fn delete_snapshot(&self, id: &str, force: bool) -> VmResult<()> {
        let rec = self
            .registry()
            .snapshots
            .get(id)
            .cloned()
            .ok_or_else(|| VmError::NotFound(format!("snapshot {id}")))?;
        if rec.template.is_some() && !force {
            return Err(VmError::State {
                name: rec.snap.vm.clone(),
                msg: "it is a template's snapshot; delete the template instead".into(),
            });
        }
        let r = self.backend.delete_snapshot(&rec.snap);
        let sid = id.to_string();
        self.update(|reg| {
            reg.snapshots.remove(&sid);
            reg.templates.retain(|_, t| t.snapshot.id != sid);
        })?;
        match r {
            Err(VmError::NotFound(_)) => Ok(()),
            other => other,
        }
    }

    pub fn delete_template(&self, key: &str) -> VmResult<()> {
        let t = self
            .registry()
            .templates
            .get(key)
            .cloned()
            .ok_or_else(|| VmError::NotFound(format!("template {key}")))?;
        self.delete_snapshot(&t.snapshot.id, true)
    }

    /// VMs owned by `task`.
    pub fn vms_of(&self, task: &str) -> Vec<VmRecord> {
        self.registry()
            .vms
            .values()
            .filter(|v| v.task.as_deref() == Some(task))
            .cloned()
            .collect()
    }
}

// ---- the runner -----------------------------------------------------------------------------

/// Runner for a task's VM: pane commands run through the backend's exec channel.
pub struct VmBoxRunner {
    pub backend: Arc<dyn VmBackend>,
    pub vm: String,
    /// The task checkout on the host and where it is mounted in the VM.
    pub checkout: PathBuf,
    pub network: NetworkProfile,
    pub proxy: bool,
    pub shell: String,
}

/// Map a host path under `checkout` to the VM workspace; anything else maps to the workspace
/// root (host paths do not exist in the VM).
pub fn map_cwd(checkout: &Path, cwd: &Path) -> String {
    match cwd.strip_prefix(checkout) {
        Ok(rel) if rel.as_os_str().is_empty() => VM_WORKSPACE.to_string(),
        Ok(rel)
            if !rel
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir)) =>
        {
            format!("{VM_WORKSPACE}/{}", rel.display())
        }
        _ => VM_WORKSPACE.to_string(),
    }
}

impl Runner for VmBoxRunner {
    fn level(&self) -> IsolationLevel {
        IsolationLevel::Vm
    }
    fn provider(&self) -> &'static str {
        self.backend.kind().as_str()
    }
    fn check(&self) -> Result<(), RunnerError> {
        self.backend.check().map(|_| ()).map_err(Into::into)
    }
    fn prepare(&self, req: SpawnRequest) -> Result<PreparedSpawn, RunnerError> {
        self.check()?;
        match self.backend.state(&self.vm) {
            Ok(VmState::Running) => {}
            Ok(s) => {
                return Err(RunnerError::Unavailable {
                    level: "vm",
                    reason: format!("vm {} is {}", self.vm, s.as_str()),
                });
            }
            Err(e) => return Err(e.into()),
        }
        // Only the pane identity and terminal capability variables cross into the VM.
        let mut env: Vec<(String, String)> = req
            .env
            .iter()
            .filter(|(k, _)| {
                matches!(
                    k.as_str(),
                    "TERM"
                        | "COLORTERM"
                        | "TERM_PROGRAM"
                        | "TERM_PROGRAM_VERSION"
                        | "LANG"
                        | "VIBEKE"
                        | "VIBEKE_PANE_ID"
                        | "VIBEKE_PANE_ULID"
                        | "VIBEKE_WORKSPACE_ID"
                        | "VIBEKE_TAB_ID"
                        | "VIBEKE_SESSION"
                ) || k.starts_with("LC_")
            })
            .cloned()
            .collect();
        crate::env::set(&mut env, "VIBEKE_ISOLATION", "vm");
        crate::env::set(&mut env, "VIBEKE_NETWORK", self.network.as_str());
        if self.network.uses_proxy() && self.proxy {
            crate::env::proxy_env(&mut env, VM_PROXY_PORT);
        }
        let cwd = map_cwd(&self.checkout, &req.cwd);
        let argv = if req.argv.is_empty() {
            vec![self.shell.clone()]
        } else {
            req.argv.clone()
        };
        let exec = self.backend.exec_argv(&self.vm, &argv, Some(&cwd), &env);
        Ok(PreparedSpawn {
            argv: exec,
            cwd: req.cwd,
            // The host-side process (limactl, tart, sh) gets a minimal env; the VM's env was
            // placed in the exec argv above.
            env: crate::env::scrub(&req.env),
            mounts: vec![Mount {
                host: self.checkout.clone(),
                target: PathBuf::from(VM_WORKSPACE),
                read_only: false,
            }],
            profile: None,
            broker_socket: None,
            visible_roots: vec![VM_WORKSPACE.to_string()],
            policy: None,
        })
    }
}

/// Snapshot and fork on a runner (13 §9, "branch this agent at turn N" groundwork).
pub trait SnapshotRunner: Runner {
    fn snapshot(&self, label: &str) -> Result<SnapshotInfo, RunnerError>;
    fn fork(&self, snap: &SnapshotInfo, spec: &VmSpec) -> Result<VmInfo, RunnerError>;
}

impl SnapshotRunner for VmBoxRunner {
    fn snapshot(&self, label: &str) -> Result<SnapshotInfo, RunnerError> {
        self.backend.snapshot(&self.vm, label).map_err(Into::into)
    }
    fn fork(&self, snap: &SnapshotInfo, spec: &VmSpec) -> Result<VmInfo, RunnerError> {
        self.backend.fork(snap, spec).map_err(Into::into)
    }
}

/// The backend a config value names, built over `state_dir` (the fake backend's root).
/// `auto` prefers Tart, then Lima, on macOS; elsewhere nothing.
pub fn backend_for(provider: &str, state_dir: &Path) -> VmResult<Arc<dyn VmBackend>> {
    let kind = match provider {
        "auto" | "" => {
            if crate::vm_backends::which("tart").is_some() {
                VmProviderKind::Tart
            } else if crate::vm_backends::which("limactl").is_some() {
                VmProviderKind::Lima
            } else {
                return Err(VmError::Unavailable(
                    "no VM provider found (install Tart or Lima on macOS; Firecracker needs explicit setup)".into(),
                ));
            }
        }
        p => VmProviderKind::parse(p)
            .ok_or_else(|| VmError::Invalid(format!("unknown vm provider `{p}`")))?,
    };
    Ok(match kind {
        VmProviderKind::Fake => Arc::new(FakeVmBackend::new(&state_dir.join("vm-fake"))),
        VmProviderKind::Lima => Arc::new(crate::vm_backends::LimaBackend::system()),
        VmProviderKind::Tart => Arc::new(crate::vm_backends::TartBackend::system()),
        VmProviderKind::Firecracker | VmProviderKind::CloudHypervisor => {
            return Err(VmError::Unavailable(format!(
                "{} is scaffolding only (configuration generation); use lima or tart",
                kind.as_str()
            )));
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake() -> (tempfile::TempDir, Arc<FakeVmBackend>) {
        let d = tempfile::tempdir().unwrap();
        let b = Arc::new(FakeVmBackend::new(d.path()));
        (d, b)
    }

    fn checkout(d: &Path) -> PathBuf {
        let c = d.join("checkout");
        std::fs::create_dir_all(c.join("src")).unwrap();
        std::fs::write(c.join("src/a.txt"), "hello").unwrap();
        c
    }

    fn spec(name: &str, c: &Path) -> VmSpec {
        let mut s = VmSpec::new(name);
        s.mounts.push(VmMount {
            host: c.to_path_buf(),
            target: VM_WORKSPACE.into(),
            read_only: false,
        });
        s
    }

    #[test]
    fn names_and_kinds() {
        assert!(valid_name("vk-abc-1"));
        assert!(!valid_name("-x") && !valid_name("A") && !valid_name("a/b") && !valid_name(""));
        assert_eq!(VmProviderKind::parse("lima"), Some(VmProviderKind::Lima));
        assert_eq!(
            VmProviderKind::parse("cloud_hypervisor"),
            Some(VmProviderKind::CloudHypervisor)
        );
        assert_eq!(VmProviderKind::parse("qemu"), None);
        assert!(VmProviderKind::Firecracker.memory_snapshots());
        assert!(!VmProviderKind::Tart.memory_snapshots());
    }

    #[test]
    fn fake_lifecycle_enforces_states() {
        let (d, b) = fake();
        let c = checkout(d.path());
        let info = b.create(&spec("vm1", &c)).unwrap();
        assert_eq!(info.state, VmState::Created);
        assert!(b.create(&spec("vm1", &c)).is_err(), "duplicate");
        assert!(b.create(&VmSpec::new("Bad Name")).is_err());
        assert!(b.suspend("vm1").is_err(), "not running yet");
        b.start("vm1").unwrap();
        assert_eq!(b.state("vm1").unwrap(), VmState::Running);
        assert!(b.start("vm1").is_err(), "already running");
        b.suspend("vm1").unwrap();
        assert_eq!(b.state("vm1").unwrap(), VmState::Suspended);
        assert!(
            b.start("vm1").is_err(),
            "suspended VMs resume, they do not start"
        );
        b.resume("vm1").unwrap();
        b.stop("vm1").unwrap();
        b.start("vm1").unwrap();
        // The checkout is shared in, not copied.
        assert_eq!(
            std::fs::read_to_string(b.disk("vm1").join("workspace/src/a.txt")).unwrap(),
            "hello"
        );
        b.destroy("vm1").unwrap();
        assert!(matches!(b.state("vm1"), Err(VmError::NotFound(_))));
        assert!(matches!(b.destroy("vm1"), Err(VmError::NotFound(_))));
        // Destroying never touched the shared checkout.
        assert!(c.join("src/a.txt").exists());
    }

    #[test]
    fn fake_exec_runs_inside_the_vm_directory() {
        let (d, b) = fake();
        let c = checkout(d.path());
        b.create(&spec("vm1", &c)).unwrap();
        b.start("vm1").unwrap();
        let argv = b.exec_argv(
            "vm1",
            &[
                "sh".into(),
                "-c".into(),
                "pwd; echo $VIBEKE_VM $FOO; cat src/a.txt".into(),
            ],
            Some("/workspace"),
            &[("FOO".into(), "bar".into())],
        );
        let o = std::process::Command::new(&argv[0])
            .args(&argv[1..])
            .output()
            .unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        let out = String::from_utf8_lossy(&o.stdout).into_owned();
        assert!(out.contains("vm1 bar"), "{out}");
        assert!(out.ends_with("hello"), "{out}");
        assert!(out.contains("/vms/vm1/disk/workspace"), "{out}");
    }

    #[test]
    fn snapshot_and_fork_copy_the_disk_but_not_the_mounts() {
        let (d, b) = fake();
        let c = checkout(d.path());
        b.create(&spec("base", &c)).unwrap();
        b.start("base").unwrap();
        std::fs::write(b.disk("base").join("installed-deps"), "node_modules").unwrap();
        let snap = b.snapshot("base", "t1").unwrap();
        assert_eq!(snap.id, "base/t1");
        assert!(!snap.memory);
        assert!(b.snapshot("base", "t1").is_err(), "label exists");
        assert!(b.snapshot("base", "bad label").is_err());
        // Later changes to the base do not reach the fork.
        std::fs::write(b.disk("base").join("later"), "x").unwrap();
        let c2 = d.path().join("checkout2");
        std::fs::create_dir_all(&c2).unwrap();
        std::fs::write(c2.join("other.txt"), "o").unwrap();
        let f = b.fork(&snap, &spec("child", &c2)).unwrap();
        assert_eq!(f.forked_from.as_deref(), Some("base/t1"));
        assert_eq!(f.state, VmState::Created);
        assert_eq!(
            std::fs::read_to_string(b.disk("child").join("installed-deps")).unwrap(),
            "node_modules"
        );
        assert!(!b.disk("child").join("later").exists());
        // The child's workspace is its own checkout, not the base's.
        assert!(b.disk("child").join("workspace/other.txt").exists());
        assert!(!b.disk("child").join("workspace/src").exists());
        assert!(b.fork(&snap, &spec("child", &c2)).is_err(), "name taken");
        b.delete_snapshot(&snap).unwrap();
        assert!(b.fork(&snap, &spec("child2", &c2)).is_err());
        // A provider with memory snapshots records it for running VMs only.
        let mut m = FakeVmBackend::new(&d.path().join("mem"));
        m.memory_snapshots = true;
        m.create(&spec("a", &c)).unwrap();
        assert!(!m.snapshot("a", "cold").unwrap().memory);
        m.start("a").unwrap();
        assert!(m.snapshot("a", "warm").unwrap().memory);
    }

    #[test]
    fn injected_failures_surface_once() {
        let (_d, b) = fake();
        b.fail_next("create", "disk full");
        let e = b.create(&VmSpec::new("x1")).unwrap_err();
        assert!(e.to_string().contains("disk full"));
        assert!(b.create(&VmSpec::new("x1")).is_ok());
        assert!(b.calls().contains(&"create:x1".to_string()));
    }

    fn manager(d: &Path) -> (VmManager, Arc<FakeVmBackend>) {
        let b = Arc::new(FakeVmBackend::new(&d.join("fake")));
        (VmManager::new(b.clone(), &d.join("state")), b)
    }

    fn ts(setup: &[&str]) -> TemplateSpec {
        TemplateSpec {
            image: Some("ubuntu".into()),
            setup: setup.iter().map(|s| s.to_string()).collect(),
            cpus: 2,
            memory_mb: 2048,
            disk_gb: 10,
        }
    }

    #[test]
    fn template_keys_follow_their_inputs() {
        assert_eq!(ts(&["a"]).key(), ts(&["a"]).key());
        assert_ne!(ts(&["a"]).key(), ts(&["b"]).key());
        let mut t = ts(&["a"]);
        t.cpus = 8;
        assert_ne!(t.key(), ts(&["a"]).key());
        assert_eq!(ts(&["a"]).key().len(), 12);
    }

    #[test]
    fn templates_are_built_once_and_forks_start_from_them() {
        let d = tempfile::tempdir().unwrap();
        let (m, b) = manager(d.path());
        let built = std::sync::atomic::AtomicU32::new(0);
        let setup = |be: &dyn VmBackend, name: &str| -> VmResult<()> {
            built.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            // "install dependencies" on the builder's disk
            let argv = be.exec_argv(
                name,
                &["sh".into(), "-c".into(), "echo deps > deps.txt".into()],
                None,
                &[],
            );
            let o = std::process::Command::new(&argv[0])
                .args(&argv[1..])
                .output()?;
            if o.status.success() {
                Ok(())
            } else {
                Err(VmError::Command {
                    what: "setup".into(),
                    detail: String::from_utf8_lossy(&o.stderr).into(),
                })
            }
        };
        let t = m.ensure_template(&ts(&["npm ci"]), &setup).unwrap();
        assert_eq!(built.load(std::sync::atomic::Ordering::SeqCst), 1);
        // The builder VM is gone; the template's snapshot remains.
        assert!(matches!(
            b.state(&format!("vk-tpl-{}", t.key)),
            Err(VmError::NotFound(_))
        ));
        let again = m.ensure_template(&ts(&["npm ci"]), &setup).unwrap();
        assert_eq!(built.load(std::sync::atomic::Ordering::SeqCst), 1, "cached");
        assert_eq!(again.key, t.key);
        // A different setup is a different template.
        m.ensure_template(&ts(&["pnpm i"]), &setup).unwrap();
        assert_eq!(built.load(std::sync::atomic::Ordering::SeqCst), 2);
        // Claiming forks, boots and records ownership.
        let c = checkout(d.path());
        let (vm, ms) = m
            .claim_from_template(&t.key, Some("task-1"), &spec("vk-task-1", &c))
            .unwrap();
        assert_eq!(vm.state, VmState::Running);
        assert!(ms < 5000);
        assert_eq!(
            std::fs::read_to_string(b.disk("vk-task-1").join("deps.txt")).unwrap(),
            "deps\n"
        );
        assert_eq!(m.vms_of("task-1").len(), 1);
        assert_eq!(
            m.registry().vms["vk-task-1"].template.as_deref(),
            Some(t.key.as_str())
        );
        assert!(m.claim_from_template("nope", None, &spec("x", &c)).is_err());
        // A failed setup leaves nothing behind.
        let bad = |_: &dyn VmBackend, _: &str| -> VmResult<()> {
            Err(VmError::Command {
                what: "setup".into(),
                detail: "boom".into(),
            })
        };
        assert!(m.ensure_template(&ts(&["broken"]), &bad).is_err());
        assert!(!m.registry().templates.contains_key(&ts(&["broken"]).key()));
        assert!(matches!(
            b.state(&format!("vk-tpl-{}", ts(&["broken"]).key())),
            Err(VmError::NotFound(_))
        ));
    }

    #[test]
    fn best_of_n_forks_share_one_snapshot_and_roll_back_on_failure() {
        let d = tempfile::tempdir().unwrap();
        let (m, b) = manager(d.path());
        let c = checkout(d.path());
        m.create(Some("parent"), &spec("vk-parent", &c)).unwrap();
        std::fs::write(b.disk("vk-parent").join("state"), "turn-3").unwrap();
        let snap = m.snapshot("vk-parent", "base").unwrap();
        let specs: Vec<(Option<String>, VmSpec)> = (1..=3)
            .map(|i| {
                let cc = d.path().join(format!("c{i}"));
                std::fs::create_dir_all(&cc).unwrap();
                (
                    Some(format!("task-{i}")),
                    spec(&format!("vk-child-{i}"), &cc),
                )
            })
            .collect();
        let vms = m.fork_many(&snap.id, &specs).unwrap();
        assert_eq!(vms.len(), 3);
        for i in 1..=3 {
            assert_eq!(
                std::fs::read_to_string(b.disk(&format!("vk-child-{i}")).join("state")).unwrap(),
                "turn-3"
            );
        }
        assert_eq!(m.vms_of("task-2").len(), 1);
        // All or nothing: the third VM fails to start, the first two are destroyed again.
        let mut bad = specs.clone();
        for (i, (_, s)) in bad.iter_mut().enumerate() {
            s.name = format!("vk-again-{i}");
        }
        b.fail_nth("start", 2, "no capacity");
        assert!(m.fork_many(&snap.id, &bad).is_err());
        assert!(m.registry().vms.keys().all(|k| !k.starts_with("vk-again")));
        for i in 0..3 {
            assert!(
                matches!(b.state(&format!("vk-again-{i}")), Err(VmError::NotFound(_))),
                "rolled back {i}"
            );
        }
        assert!(m.fork_many("nope/x", &specs).is_err());
        // Destroying updates the registry.
        m.destroy("vk-child-1").unwrap();
        assert!(!m.registry().vms.contains_key("vk-child-1"));
        m.destroy("vk-child-1").unwrap();
        // Template snapshots are protected from a plain delete.
        let t = m.ensure_template(&ts(&["x"]), &|_, _| Ok(())).unwrap();
        assert!(m.delete_snapshot(&t.snapshot.id, false).is_err());
        m.delete_template(&t.key).unwrap();
        assert!(!m.registry().templates.contains_key(&t.key));
        m.delete_snapshot(&snap.id, false).unwrap();
    }

    #[test]
    fn runner_wraps_pane_commands_for_a_running_vm() {
        let d = tempfile::tempdir().unwrap();
        let (m, b) = manager(d.path());
        let c = checkout(d.path());
        m.create(Some("t"), &spec("vk-t", &c)).unwrap();
        let r = VmBoxRunner {
            backend: b.clone(),
            vm: "vk-t".into(),
            checkout: c.clone(),
            network: NetworkProfile::Dev,
            proxy: true,
            shell: "/bin/sh".into(),
        };
        assert_eq!(r.level(), IsolationLevel::Vm);
        assert_eq!(r.provider(), "fake");
        r.check().unwrap();
        let p = r
            .prepare(SpawnRequest {
                pane_id: "p1".into(),
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "echo $VIBEKE_ISOLATION $HTTPS_PROXY $SECRET; pwd".into(),
                ],
                cwd: c.join("src"),
                env: vec![
                    ("VIBEKE_PANE_ID".into(), "p1".into()),
                    ("SECRET".into(), "host-secret".into()),
                    ("TERM".into(), "xterm".into()),
                ],
            })
            .unwrap();
        assert_eq!(p.mounts.len(), 1);
        assert_eq!(p.visible_roots, vec!["/workspace"]);
        let o = std::process::Command::new(&p.argv[0])
            .args(&p.argv[1..])
            .output()
            .unwrap();
        let out = String::from_utf8_lossy(&o.stdout).into_owned();
        assert!(out.starts_with("vm "), "{out}");
        assert!(out.contains("127.0.0.1:3128"), "proxy env set: {out}");
        assert!(
            !out.contains("host-secret"),
            "host env must not cross into the VM: {out}"
        );
        assert!(out.contains("/disk/workspace/src"), "{out}");
        // No argv: the shell.
        let p = r
            .prepare(SpawnRequest {
                pane_id: "p2".into(),
                argv: vec![],
                cwd: c.clone(),
                env: vec![],
            })
            .unwrap();
        assert!(p.argv.contains(&"/bin/sh".to_string()));
        // A stopped VM cannot spawn.
        b.stop("vk-t").unwrap();
        assert!(
            r.prepare(SpawnRequest {
                pane_id: "p3".into(),
                argv: vec!["ls".into()],
                cwd: c.clone(),
                env: vec![]
            })
            .is_err()
        );
        // Snapshot and fork through the runner.
        b.start("vk-t").unwrap();
        let s = SnapshotRunner::snapshot(&r, "via-runner").unwrap();
        let f = SnapshotRunner::fork(&r, &s, &spec("vk-fork", &c)).unwrap();
        assert_eq!(f.forked_from.as_deref(), Some("vk-t/via-runner"));
    }

    #[test]
    fn cwd_mapping() {
        let c = Path::new("/work/task");
        assert_eq!(map_cwd(c, Path::new("/work/task")), "/workspace");
        assert_eq!(
            map_cwd(c, Path::new("/work/task/src/a")),
            "/workspace/src/a"
        );
        assert_eq!(map_cwd(c, Path::new("/elsewhere")), "/workspace");
        assert_eq!(map_cwd(c, Path::new("/work/task/../x")), "/workspace");
    }

    #[test]
    fn firecracker_generates_config_and_reports_unavailable() {
        let f = FirecrackerBackend {
            kernel: "/k/vmlinux".into(),
            rootfs: "/k/rootfs.ext4".into(),
            bin: "/nonexistent/firecracker".into(),
        };
        let c = f.machine_config(&VmSpec::new("a"), 7, Path::new("/run/v.sock"));
        assert_eq!(c["machine-config"]["vcpu_count"], 4);
        assert_eq!(c["vsock"]["guest_cid"], 7);
        assert_eq!(c["drives"][0]["is_root_device"], true);
        let s = f.snapshot_create_body(Path::new("/snap/x"));
        assert_eq!(s["snapshot_type"], "Full");
        assert_eq!(s["mem_file_path"], "/snap/x/vm.mem");
        let l = f.snapshot_load_body(Path::new("/snap/x"));
        assert_eq!(l["resume_vm"], true);
        assert!(f.check().is_err());
    }

    #[test]
    fn backend_selection() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(
            backend_for("fake", d.path()).unwrap().kind(),
            VmProviderKind::Fake
        );
        assert!(backend_for("qemu", d.path()).is_err());
        assert!(backend_for("firecracker", d.path()).is_err());
    }
}
