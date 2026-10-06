//! Real VM provider command lines: Lima (`limactl`, `vz` on macOS) and Tart.
//!
//! **Unverified against real VMs.** Both backends build their commands from the providers'
//! documented CLIs and are tested against [`FakeCmd`], a recording command runner. Running them
//! needs Apple Silicon (or a Linux KVM host for Lima `qemu`); that is on the gap audit's
//! "needs the user" list. Where a provider cannot do something (Lima has no suspend with save
//! state; neither clones a *running* disk safely) the backend does the closest safe thing and
//! says so in the method docs.

use crate::vm::*;
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .chain(["/usr/local/bin".into(), "/opt/homebrew/bin".into()])
        .map(|d| d.join(name))
        .find(|p| p.is_file())
}

#[derive(Debug, Clone, Default)]
pub struct CmdOut {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOut {
    pub fn ok(stdout: &str) -> CmdOut {
        CmdOut {
            ok: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }
    pub fn fail(stderr: &str) -> CmdOut {
        CmdOut {
            ok: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }
}

/// Runs provider CLIs. `run` waits; `spawn_detached` starts a long-lived process (a Tart VM
/// is the `tart run` process itself).
pub trait CmdRunner: Send + Sync {
    fn run(&self, argv: &[String]) -> std::io::Result<CmdOut>;
    fn spawn_detached(&self, argv: &[String]) -> std::io::Result<()>;
}

pub struct SysCmd;

impl CmdRunner for SysCmd {
    fn run(&self, argv: &[String]) -> std::io::Result<CmdOut> {
        let o = Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .output()?;
        Ok(CmdOut {
            ok: o.status.success(),
            code: o.status.code(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).trim().to_string(),
        })
    }
    fn spawn_detached(&self, argv: &[String]) -> std::io::Result<()> {
        Command::new(&argv[0])
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map(|_| ())
    }
}

/// Records every command; answers from a queue (default: success with empty output).
#[derive(Default)]
pub struct FakeCmd {
    pub calls: Mutex<Vec<Vec<String>>>,
    pub detached: Mutex<Vec<Vec<String>>>,
    replies: Mutex<VecDeque<CmdOut>>,
}

impl FakeCmd {
    pub fn reply(&self, o: CmdOut) {
        self.replies.lock().unwrap().push_back(o);
    }
    pub fn argvs(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|a| a.join(" "))
            .collect()
    }
}

impl CmdRunner for FakeCmd {
    fn run(&self, argv: &[String]) -> std::io::Result<CmdOut> {
        self.calls.lock().unwrap().push(argv.to_vec());
        Ok(self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or_else(|| CmdOut::ok("")))
    }
    fn spawn_detached(&self, argv: &[String]) -> std::io::Result<()> {
        self.detached.lock().unwrap().push(argv.to_vec());
        Ok(())
    }
}

fn cmd_err(what: &str, o: &CmdOut) -> VmError {
    VmError::Command {
        what: what.to_string(),
        detail: if o.stderr.is_empty() {
            format!("exit {:?}", o.code)
        } else {
            o.stderr.clone()
        },
    }
}

fn run_ok(c: &dyn CmdRunner, what: &str, argv: Vec<String>) -> VmResult<CmdOut> {
    let o = c.run(&argv)?;
    if o.ok { Ok(o) } else { Err(cmd_err(what, &o)) }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// A provider instance name for a VM or snapshot, so Vibeke's instances are recognizable and
/// never collide with the user's own.
fn inst(name: &str) -> String {
    format!("vibeke-{name}")
}
fn snap_inst(vm: &str, label: &str) -> String {
    format!("vibeke-snap-{vm}-{label}")
}

// ---- Lima ------------------------------------------------------------------------------------

pub struct LimaBackend {
    bin: String,
    cmd: Box<dyn CmdRunner>,
    /// Where generated instance YAML files go.
    config_dir: PathBuf,
    /// `vz` (macOS) or `qemu`.
    vm_type: String,
}

impl LimaBackend {
    pub fn system() -> LimaBackend {
        LimaBackend {
            bin: which("limactl")
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "limactl".into()),
            cmd: Box::new(SysCmd),
            config_dir: std::env::temp_dir().join("vibeke-lima"),
            vm_type: if cfg!(target_os = "macos") {
                "vz".into()
            } else {
                "qemu".into()
            },
        }
    }
    pub fn with(
        bin: &str,
        cmd: Box<dyn CmdRunner>,
        config_dir: &Path,
        vm_type: &str,
    ) -> LimaBackend {
        LimaBackend {
            bin: bin.into(),
            cmd,
            config_dir: config_dir.into(),
            vm_type: vm_type.into(),
        }
    }
    fn c(&self, args: &[&str]) -> Vec<String> {
        let mut v = vec![self.bin.clone()];
        v.extend(args.iter().map(|a| a.to_string()));
        v
    }
    /// The instance YAML for `spec`: size, mounts (the checkout at `/workspace`, not at its
    /// host path), no container runtimes.
    pub fn yaml(&self, spec: &VmSpec) -> String {
        let image = spec
            .image
            .clone()
            .unwrap_or_else(|| "template:ubuntu".into());
        let mut y = format!(
            "vmType: {}\ncpus: {}\nmemory: \"{}MiB\"\ndisk: \"{}GiB\"\ncontainerd:\n  system: false\n  user: false\n",
            self.vm_type, spec.cpus, spec.memory_mb, spec.disk_gb
        );
        if let Some(rest) = image.strip_prefix("template:") {
            y.push_str(&format!("base: template://{rest}\n"));
        } else {
            y.push_str(&format!("images:\n  - location: \"{image}\"\n"));
        }
        if !spec.mounts.is_empty() {
            y.push_str("mounts:\n");
            for m in &spec.mounts {
                y.push_str(&format!(
                    "  - location: \"{}\"\n    mountPoint: \"{}\"\n    writable: {}\n",
                    m.host.display(),
                    m.target,
                    !m.read_only
                ));
            }
        }
        y
    }
    fn status(&self, name: &str) -> VmResult<Option<String>> {
        let o = run_ok(
            self.cmd.as_ref(),
            "limactl list",
            self.c(&["list", "--format", "json", &inst(name)]),
        )?;
        for line in o.stdout.lines().filter(|l| !l.trim().is_empty()) {
            let v: Value =
                serde_json::from_str(line).map_err(|e| VmError::Invalid(e.to_string()))?;
            if v["name"] == inst(name) {
                return Ok(v["status"].as_str().map(str::to_string));
            }
        }
        Ok(None)
    }
}

impl VmBackend for LimaBackend {
    fn kind(&self) -> VmProviderKind {
        VmProviderKind::Lima
    }
    fn check(&self) -> VmResult<String> {
        let o = self
            .cmd
            .run(&self.c(&["--version"]))
            .map_err(|_| VmError::Unavailable("limactl not found".into()))?;
        if o.ok {
            Ok(format!(
                "{} ({}, vm type {})",
                o.stdout.trim(),
                self.bin,
                self.vm_type
            ))
        } else {
            Err(VmError::Unavailable(format!(
                "limactl failed: {}",
                o.stderr
            )))
        }
    }
    fn create(&self, spec: &VmSpec) -> VmResult<VmInfo> {
        if !valid_name(&spec.name) {
            return Err(VmError::Invalid(format!(
                "`{}` is not a valid vm name",
                spec.name
            )));
        }
        std::fs::create_dir_all(&self.config_dir)?;
        let file = self.config_dir.join(format!("{}.yaml", spec.name));
        std::fs::write(&file, self.yaml(spec))?;
        run_ok(
            self.cmd.as_ref(),
            "limactl create",
            self.c(&[
                "create",
                &format!("--name={}", inst(&spec.name)),
                "--tty=false",
                &file.to_string_lossy(),
            ]),
        )?;
        Ok(VmInfo {
            name: spec.name.clone(),
            provider: VmProviderKind::Lima,
            state: VmState::Created,
            cpus: spec.cpus,
            memory_mb: spec.memory_mb,
            created_ms: now_ms(),
            forked_from: None,
        })
    }
    fn start(&self, name: &str) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "limactl start",
            self.c(&["start", "--tty=false", &inst(name)]),
        )
        .map(|_| ())
    }
    fn stop(&self, name: &str) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "limactl stop",
            self.c(&["stop", &inst(name)]),
        )
        .map(|_| ())
    }
    /// Lima has no save-state suspend: this stops the instance (the disk is kept), and
    /// `resume` starts it again. Memory is not preserved.
    fn suspend(&self, name: &str) -> VmResult<()> {
        self.stop(name)
    }
    fn resume(&self, name: &str) -> VmResult<()> {
        self.start(name)
    }
    fn destroy(&self, name: &str) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "limactl delete",
            self.c(&["delete", "--force", &inst(name)]),
        )
        .map(|_| ())?;
        let _ = std::fs::remove_file(self.config_dir.join(format!("{name}.yaml")));
        Ok(())
    }
    fn state(&self, name: &str) -> VmResult<VmState> {
        match self.status(name)?.as_deref() {
            Some("Running") => Ok(VmState::Running),
            Some("Stopped") => Ok(VmState::Stopped),
            Some(_) => Ok(VmState::Created),
            None => Err(VmError::NotFound(name.to_string())),
        }
    }
    /// `limactl clone` needs a stopped source, so a running VM is stopped, cloned and started
    /// again (a short pause; the clone is a disk snapshot, `memory` is false).
    fn snapshot(&self, name: &str, label: &str) -> VmResult<SnapshotInfo> {
        let was_running = self.state(name)? == VmState::Running;
        if was_running {
            self.stop(name)?;
        }
        let cloned = run_ok(
            self.cmd.as_ref(),
            "limactl clone",
            self.c(&["clone", &inst(name), &snap_inst(name, label), "--tty=false"]),
        );
        if was_running {
            self.start(name)?;
        }
        cloned?;
        Ok(SnapshotInfo {
            id: format!("{name}/{label}"),
            vm: name.into(),
            label: label.into(),
            memory: false,
            created_ms: now_ms(),
            provider: VmProviderKind::Lima,
        })
    }
    fn fork(&self, snap: &SnapshotInfo, spec: &VmSpec) -> VmResult<VmInfo> {
        if !valid_name(&spec.name) {
            return Err(VmError::Invalid(format!(
                "`{}` is not a valid vm name",
                spec.name
            )));
        }
        // Mounts of the fork replace the source's (the checkout differs per task).
        let mounts: Vec<String> = spec
            .mounts
            .iter()
            .map(|m| {
                format!(
                    "{{\"location\":\"{}\",\"mountPoint\":\"{}\",\"writable\":{}}}",
                    m.host.display(),
                    m.target,
                    !m.read_only
                )
            })
            .collect();
        let set = format!(
            ".mounts = [{}] | .cpus = {} | .memory = \"{}MiB\"",
            mounts.join(","),
            spec.cpus,
            spec.memory_mb
        );
        run_ok(
            self.cmd.as_ref(),
            "limactl clone",
            self.c(&[
                "clone",
                &snap_inst(&snap.vm, &snap.label),
                &inst(&spec.name),
                "--tty=false",
                "--set",
                &set,
            ]),
        )?;
        Ok(VmInfo {
            name: spec.name.clone(),
            provider: VmProviderKind::Lima,
            state: VmState::Stopped,
            cpus: spec.cpus,
            memory_mb: spec.memory_mb,
            created_ms: now_ms(),
            forked_from: Some(snap.id.clone()),
        })
    }
    fn delete_snapshot(&self, snap: &SnapshotInfo) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "limactl delete",
            self.c(&["delete", "--force", &snap_inst(&snap.vm, &snap.label)]),
        )
        .map(|_| ())
    }
    fn exec_argv(
        &self,
        name: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Vec<String> {
        let mut v = vec![self.bin.clone(), "shell".into()];
        if let Some(c) = cwd {
            v.push("--workdir".into());
            v.push(c.into());
        }
        v.push(inst(name));
        v.push("--".into());
        v.push("env".into());
        v.extend(env.iter().map(|(k, val)| format!("{k}={val}")));
        v.extend(argv.iter().cloned());
        v
    }
    fn transports(&self, _name: &str) -> Vec<VmTransportKind> {
        // `limactl shell` rides on the instance's ssh; the vz driver also has a vsock device
        // that Lima does not expose to other processes.
        vec![VmTransportKind::Exec, VmTransportKind::Ssh]
    }
}

// ---- Tart ------------------------------------------------------------------------------------

pub struct TartBackend {
    bin: String,
    cmd: Box<dyn CmdRunner>,
    /// Where each VM's spec (its mounts) is remembered across restarts.
    spec_dir: PathBuf,
    specs: Mutex<BTreeMap<String, VmSpec>>,
}

impl TartBackend {
    pub fn system() -> TartBackend {
        TartBackend::with(
            &which("tart")
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "tart".into()),
            Box::new(SysCmd),
            &std::env::temp_dir().join("vibeke-tart"),
        )
    }
    pub fn with(bin: &str, cmd: Box<dyn CmdRunner>, spec_dir: &Path) -> TartBackend {
        TartBackend {
            bin: bin.into(),
            cmd,
            spec_dir: spec_dir.into(),
            specs: Mutex::new(BTreeMap::new()),
        }
    }
    fn c(&self, args: &[&str]) -> Vec<String> {
        let mut v = vec![self.bin.clone()];
        v.extend(args.iter().map(|a| a.to_string()));
        v
    }
    fn remember(&self, spec: &VmSpec) -> VmResult<()> {
        std::fs::create_dir_all(&self.spec_dir)?;
        std::fs::write(
            self.spec_dir.join(format!("{}.json", spec.name)),
            serde_json::to_vec(spec).map_err(|e| VmError::Invalid(e.to_string()))?,
        )?;
        self.specs
            .lock()
            .unwrap()
            .insert(spec.name.clone(), spec.clone());
        Ok(())
    }
    fn spec_of(&self, name: &str) -> Option<VmSpec> {
        if let Some(s) = self.specs.lock().unwrap().get(name) {
            return Some(s.clone());
        }
        let b = std::fs::read(self.spec_dir.join(format!("{name}.json"))).ok()?;
        serde_json::from_slice(&b).ok()
    }
    fn set_size(&self, spec: &VmSpec) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "tart set",
            self.c(&[
                "set",
                &inst(&spec.name),
                "--cpu",
                &spec.cpus.to_string(),
                "--memory",
                &spec.memory_mb.to_string(),
                "--disk-size",
                &spec.disk_gb.to_string(),
            ]),
        )
        .map(|_| ())
    }
    fn list(&self) -> VmResult<Vec<Value>> {
        let o = run_ok(
            self.cmd.as_ref(),
            "tart list",
            self.c(&["list", "--format", "json"]),
        )?;
        let v: Value =
            serde_json::from_str(&o.stdout).map_err(|e| VmError::Invalid(e.to_string()))?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }
}

impl VmBackend for TartBackend {
    fn kind(&self) -> VmProviderKind {
        VmProviderKind::Tart
    }
    fn check(&self) -> VmResult<String> {
        if !cfg!(target_os = "macos") {
            return Err(VmError::Unavailable(
                "Tart needs macOS on Apple Silicon".into(),
            ));
        }
        let o = self
            .cmd
            .run(&self.c(&["--version"]))
            .map_err(|_| VmError::Unavailable("tart not found".into()))?;
        if o.ok {
            Ok(format!("tart {} ({})", o.stdout.trim(), self.bin))
        } else {
            Err(VmError::Unavailable(format!("tart failed: {}", o.stderr)))
        }
    }
    fn create(&self, spec: &VmSpec) -> VmResult<VmInfo> {
        if !valid_name(&spec.name) {
            return Err(VmError::Invalid(format!(
                "`{}` is not a valid vm name",
                spec.name
            )));
        }
        let image = spec
            .image
            .clone()
            .ok_or_else(|| VmError::Invalid("tart needs an image (isolation.vm.image)".into()))?;
        run_ok(
            self.cmd.as_ref(),
            "tart clone",
            self.c(&["clone", &image, &inst(&spec.name)]),
        )?;
        self.set_size(spec)?;
        self.remember(spec)?;
        Ok(VmInfo {
            name: spec.name.clone(),
            provider: VmProviderKind::Tart,
            state: VmState::Created,
            cpus: spec.cpus,
            memory_mb: spec.memory_mb,
            created_ms: now_ms(),
            forked_from: None,
        })
    }
    /// `tart run` is the VM process: started detached with the mounts as virtiofs shares, then
    /// the shares are linked at their targets through the guest agent.
    fn start(&self, name: &str) -> VmResult<()> {
        let spec = self.spec_of(name);
        let mut run = vec![
            self.bin.clone(),
            "run".into(),
            "--no-graphics".into(),
            "--suspendable".into(),
        ];
        if let Some(sp) = &spec {
            for (i, m) in sp.mounts.iter().enumerate() {
                run.push(format!(
                    "--dir=share{i}:{}{}",
                    m.host.display(),
                    if m.read_only { ":ro" } else { "" }
                ));
            }
        }
        run.push(inst(name));
        self.cmd.spawn_detached(&run)?;
        if let Some(sp) = spec {
            for (i, m) in sp.mounts.iter().enumerate() {
                let script = format!(
                    "mkdir -p \"$(dirname '{t}')\" && ln -sfn '/Volumes/My Shared Files/share{i}' '{t}'",
                    t = m.target
                );
                run_ok(
                    self.cmd.as_ref(),
                    "tart exec",
                    self.c(&["exec", &inst(name), "sh", "-c", &script]),
                )?;
            }
        }
        Ok(())
    }
    fn stop(&self, name: &str) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "tart stop",
            self.c(&["stop", &inst(name)]),
        )
        .map(|_| ())
    }
    fn suspend(&self, name: &str) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "tart suspend",
            self.c(&["suspend", &inst(name)]),
        )
        .map(|_| ())
    }
    /// A suspendable VM restores its saved state when run again.
    fn resume(&self, name: &str) -> VmResult<()> {
        self.start(name)
    }
    fn destroy(&self, name: &str) -> VmResult<()> {
        let _ = self.cmd.run(&self.c(&["stop", &inst(name)]));
        run_ok(
            self.cmd.as_ref(),
            "tart delete",
            self.c(&["delete", &inst(name)]),
        )
        .map(|_| ())?;
        let _ = std::fs::remove_file(self.spec_dir.join(format!("{name}.json")));
        self.specs.lock().unwrap().remove(name);
        Ok(())
    }
    fn state(&self, name: &str) -> VmResult<VmState> {
        let want = inst(name);
        for v in self.list()? {
            if v["Name"] == want {
                return Ok(
                    match v["State"].as_str().map(str::to_ascii_lowercase).as_deref() {
                        Some("running") => VmState::Running,
                        Some("suspended") => VmState::Suspended,
                        Some("stopped") => VmState::Stopped,
                        _ if v["Running"].as_bool() == Some(true) => VmState::Running,
                        _ => VmState::Stopped,
                    },
                );
            }
        }
        Err(VmError::NotFound(name.to_string()))
    }
    /// A copy-on-write clone of the disk (APFS). A running VM is stopped first so the disk is
    /// consistent, then run again.
    fn snapshot(&self, name: &str, label: &str) -> VmResult<SnapshotInfo> {
        let was_running = self.state(name)? == VmState::Running;
        if was_running {
            self.stop(name)?;
        }
        let cloned = run_ok(
            self.cmd.as_ref(),
            "tart clone",
            self.c(&["clone", &inst(name), &snap_inst(name, label)]),
        );
        if was_running {
            self.start(name)?;
        }
        cloned?;
        Ok(SnapshotInfo {
            id: format!("{name}/{label}"),
            vm: name.into(),
            label: label.into(),
            memory: false,
            created_ms: now_ms(),
            provider: VmProviderKind::Tart,
        })
    }
    fn fork(&self, snap: &SnapshotInfo, spec: &VmSpec) -> VmResult<VmInfo> {
        if !valid_name(&spec.name) {
            return Err(VmError::Invalid(format!(
                "`{}` is not a valid vm name",
                spec.name
            )));
        }
        run_ok(
            self.cmd.as_ref(),
            "tart clone",
            self.c(&[
                "clone",
                &snap_inst(&snap.vm, &snap.label),
                &inst(&spec.name),
            ]),
        )?;
        self.set_size(spec)?;
        self.remember(spec)?;
        Ok(VmInfo {
            name: spec.name.clone(),
            provider: VmProviderKind::Tart,
            state: VmState::Stopped,
            cpus: spec.cpus,
            memory_mb: spec.memory_mb,
            created_ms: now_ms(),
            forked_from: Some(snap.id.clone()),
        })
    }
    fn delete_snapshot(&self, snap: &SnapshotInfo) -> VmResult<()> {
        run_ok(
            self.cmd.as_ref(),
            "tart delete",
            self.c(&["delete", &snap_inst(&snap.vm, &snap.label)]),
        )
        .map(|_| ())
    }
    fn exec_argv(
        &self,
        name: &str,
        argv: &[String],
        cwd: Option<&str>,
        env: &[(String, String)],
    ) -> Vec<String> {
        // `tart exec` has no working-directory flag: change directory inside the guest.
        let mut v = vec![
            self.bin.clone(),
            "exec".into(),
            "-i".into(),
            "-t".into(),
            inst(name),
        ];
        v.extend([
            "sh".into(),
            "-c".into(),
            "cd \"$1\" && shift && exec \"$@\"".into(),
            "vibeke-vm".into(),
            cwd.unwrap_or("/").into(),
            "env".into(),
        ]);
        v.extend(env.iter().map(|(k, val)| format!("{k}={val}")));
        v.extend(argv.iter().cloned());
        v
    }
    fn transports(&self, _name: &str) -> Vec<VmTransportKind> {
        vec![VmTransportKind::Exec, VmTransportKind::Ssh]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// A `CmdRunner` shared between a backend and the test that inspects it.
    struct Shared(Arc<FakeCmd>);
    impl CmdRunner for Shared {
        fn run(&self, a: &[String]) -> std::io::Result<CmdOut> {
            self.0.run(a)
        }
        fn spawn_detached(&self, a: &[String]) -> std::io::Result<()> {
            self.0.spawn_detached(a)
        }
    }

    fn spec(name: &str) -> VmSpec {
        let mut s = VmSpec::new(name);
        s.image = Some("template:ubuntu".into());
        s.mounts.push(VmMount {
            host: "/work/task".into(),
            target: "/workspace".into(),
            read_only: false,
        });
        s
    }

    fn lima() -> (tempfile::TempDir, Arc<FakeCmd>, LimaBackend) {
        let d = tempfile::tempdir().unwrap();
        let c = Arc::new(FakeCmd::default());
        let b = LimaBackend::with("limactl", Box::new(Shared(c.clone())), d.path(), "vz");
        (d, c, b)
    }

    #[test]
    fn lima_yaml_mounts_the_checkout_at_workspace() {
        let (_d, _c, b) = lima();
        let y = b.yaml(&spec("t1"));
        assert!(y.contains("vmType: vz"));
        assert!(y.contains("cpus: 4"));
        assert!(y.contains("memory: \"8192MiB\""));
        assert!(y.contains("base: template://ubuntu"));
        assert!(y.contains("location: \"/work/task\""));
        assert!(y.contains("mountPoint: \"/workspace\""));
        assert!(y.contains("writable: true"));
        let mut other = spec("t2");
        other.image = Some("https://example.invalid/img.qcow2".into());
        other.mounts[0].read_only = true;
        let y = b.yaml(&other);
        assert!(y.contains("images:") && y.contains("img.qcow2"));
        assert!(y.contains("writable: false"));
    }

    #[test]
    fn lima_lifecycle_commands() {
        let (d, c, b) = lima();
        b.create(&spec("t1")).unwrap();
        assert!(d.path().join("t1.yaml").exists());
        b.start("t1").unwrap();
        b.suspend("t1").unwrap();
        b.resume("t1").unwrap();
        b.destroy("t1").unwrap();
        assert!(!d.path().join("t1.yaml").exists());
        let calls = c.argvs();
        assert!(
            calls[0].starts_with("limactl create --name=vibeke-t1 --tty=false "),
            "{calls:?}"
        );
        assert!(calls[0].ends_with("t1.yaml"));
        assert_eq!(calls[1], "limactl start --tty=false vibeke-t1");
        assert_eq!(
            calls[2], "limactl stop vibeke-t1",
            "suspend is stop for Lima"
        );
        assert_eq!(calls[3], "limactl start --tty=false vibeke-t1");
        assert_eq!(calls[4], "limactl delete --force vibeke-t1");
        assert!(b.create(&VmSpec::new("Bad")).is_err());
    }

    #[test]
    fn lima_state_snapshot_and_fork() {
        let (_d, c, b) = lima();
        c.reply(CmdOut::ok(
            "{\"name\":\"vibeke-t1\",\"status\":\"Running\"}\n",
        ));
        assert_eq!(b.state("t1").unwrap(), VmState::Running);
        c.reply(CmdOut::ok(
            "{\"name\":\"vibeke-t1\",\"status\":\"Stopped\"}\n",
        ));
        assert_eq!(b.state("t1").unwrap(), VmState::Stopped);
        c.reply(CmdOut::ok(""));
        assert!(matches!(b.state("t1"), Err(VmError::NotFound(_))));
        // snapshot of a running VM: stop, clone, start
        c.calls.lock().unwrap().clear();
        c.reply(CmdOut::ok(
            "{\"name\":\"vibeke-t1\",\"status\":\"Running\"}\n",
        ));
        let snap = b.snapshot("t1", "base").unwrap();
        assert_eq!(snap.id, "t1/base");
        assert!(!snap.memory);
        let calls = c.argvs();
        assert_eq!(calls[1], "limactl stop vibeke-t1");
        assert_eq!(
            calls[2],
            "limactl clone vibeke-t1 vibeke-snap-t1-base --tty=false"
        );
        assert_eq!(calls[3], "limactl start --tty=false vibeke-t1");
        // a failed clone still restarts the source and reports the failure
        c.calls.lock().unwrap().clear();
        c.reply(CmdOut::ok(
            "{\"name\":\"vibeke-t1\",\"status\":\"Running\"}\n",
        ));
        c.reply(CmdOut::ok(""));
        c.reply(CmdOut::fail("disk full"));
        let e = b.snapshot("t1", "bad").unwrap_err();
        assert!(e.to_string().contains("disk full"));
        assert_eq!(
            c.argvs().last().unwrap(),
            "limactl start --tty=false vibeke-t1"
        );
        // fork sets the new mounts
        c.calls.lock().unwrap().clear();
        let f = b.fork(&snap, &spec("t2")).unwrap();
        assert_eq!(f.forked_from.as_deref(), Some("t1/base"));
        let call = &c.argvs()[0];
        assert!(
            call.starts_with("limactl clone vibeke-snap-t1-base vibeke-t2 --tty=false --set "),
            "{call}"
        );
        assert!(call.contains("\"mountPoint\":\"/workspace\""));
        b.delete_snapshot(&snap).unwrap();
        assert_eq!(
            c.argvs().last().unwrap(),
            "limactl delete --force vibeke-snap-t1-base"
        );
    }

    #[test]
    fn lima_exec_and_failures() {
        let (_d, c, b) = lima();
        let v = b.exec_argv(
            "t1",
            &["zsh".into(), "-l".into()],
            Some("/workspace/src"),
            &[("A".into(), "1".into())],
        );
        assert_eq!(
            v.join(" "),
            "limactl shell --workdir /workspace/src vibeke-t1 -- env A=1 zsh -l"
        );
        c.reply(CmdOut::fail("instance exists"));
        let e = b.start("t1").unwrap_err();
        assert!(e.to_string().contains("instance exists"));
        c.reply(CmdOut::ok("limactl version 1.0.0\n"));
        assert!(b.check().unwrap().contains("1.0.0"));
        c.reply(CmdOut::fail("nope"));
        assert!(b.check().is_err());
        assert_eq!(
            b.transports("t1"),
            vec![VmTransportKind::Exec, VmTransportKind::Ssh]
        );
    }

    fn tart() -> (tempfile::TempDir, Arc<FakeCmd>, TartBackend) {
        let d = tempfile::tempdir().unwrap();
        let c = Arc::new(FakeCmd::default());
        let b = TartBackend::with("tart", Box::new(Shared(c.clone())), d.path());
        (d, c, b)
    }

    #[test]
    fn tart_create_start_with_shares_and_links() {
        let (d, c, b) = tart();
        let mut sp = spec("t1");
        sp.image = Some("ghcr.io/cirruslabs/ubuntu:latest".into());
        assert!(
            b.create(&VmSpec::new("t1")).is_err(),
            "an image is required"
        );
        b.create(&sp).unwrap();
        assert!(d.path().join("t1.json").exists());
        let calls = c.argvs();
        assert_eq!(
            calls[0],
            "tart clone ghcr.io/cirruslabs/ubuntu:latest vibeke-t1"
        );
        assert_eq!(
            calls[1],
            "tart set vibeke-t1 --cpu 4 --memory 8192 --disk-size 30"
        );
        c.calls.lock().unwrap().clear();
        b.start("t1").unwrap();
        let det = c.detached.lock().unwrap().clone();
        assert_eq!(det.len(), 1);
        assert_eq!(
            det[0].join(" "),
            "tart run --no-graphics --suspendable --dir=share0:/work/task vibeke-t1"
        );
        let link = &c.argvs()[0];
        assert!(link.starts_with("tart exec vibeke-t1 sh -c "), "{link}");
        assert!(link.contains("ln -sfn '/Volumes/My Shared Files/share0' '/workspace'"));
        // the spec survives a new backend instance (server restart)
        let c2 = Arc::new(FakeCmd::default());
        let b2 = TartBackend::with("tart", Box::new(Shared(c2.clone())), d.path());
        b2.start("t1").unwrap();
        assert_eq!(c2.detached.lock().unwrap().len(), 1);
        assert!(
            c2.detached.lock().unwrap()[0]
                .join(" ")
                .contains("--dir=share0:/work/task")
        );
    }

    #[test]
    fn tart_state_snapshot_fork_and_exec() {
        let (_d, c, b) = tart();
        c.reply(CmdOut::ok("[{\"Name\":\"vibeke-t1\",\"State\":\"Running\"},{\"Name\":\"other\",\"State\":\"stopped\"}]"));
        assert_eq!(b.state("t1").unwrap(), VmState::Running);
        c.reply(CmdOut::ok(
            "[{\"Name\":\"vibeke-t1\",\"State\":\"Suspended\"}]",
        ));
        assert_eq!(b.state("t1").unwrap(), VmState::Suspended);
        c.reply(CmdOut::ok("[{\"Name\":\"vibeke-t1\",\"Running\":true}]"));
        assert_eq!(b.state("t1").unwrap(), VmState::Running);
        c.reply(CmdOut::ok("[]"));
        assert!(matches!(b.state("t1"), Err(VmError::NotFound(_))));
        // snapshot of a stopped VM: just a clone
        c.calls.lock().unwrap().clear();
        c.reply(CmdOut::ok(
            "[{\"Name\":\"vibeke-t1\",\"State\":\"stopped\"}]",
        ));
        let snap = b.snapshot("t1", "base").unwrap();
        assert_eq!(c.argvs()[1], "tart clone vibeke-t1 vibeke-snap-t1-base");
        assert_eq!(c.argvs().len(), 2);
        // fork
        c.calls.lock().unwrap().clear();
        let mut sp = spec("t2");
        sp.image = Some("x".into());
        let f = b.fork(&snap, &sp).unwrap();
        assert_eq!(f.forked_from.as_deref(), Some("t1/base"));
        assert_eq!(c.argvs()[0], "tart clone vibeke-snap-t1-base vibeke-t2");
        b.suspend("t2").unwrap();
        b.stop("t2").unwrap();
        b.destroy("t2").unwrap();
        let calls = c.argvs();
        assert!(calls.contains(&"tart suspend vibeke-t2".to_string()));
        assert!(calls.contains(&"tart delete vibeke-t2".to_string()));
        b.delete_snapshot(&snap).unwrap();
        assert_eq!(c.argvs().last().unwrap(), "tart delete vibeke-snap-t1-base");
        // exec
        let v = b.exec_argv(
            "t1",
            &["zsh".into()],
            Some("/workspace"),
            &[("A".into(), "1".into())],
        );
        assert_eq!(
            v.join(" "),
            "tart exec -i -t vibeke-t1 sh -c cd \"$1\" && shift && exec \"$@\" vibeke-vm /workspace env A=1 zsh"
        );
    }
}
