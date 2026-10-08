//! Server side of the `container` level (13 §4–§11): building a task's box from config, the
//! repo's `.vibeke/sandbox.toml` and its devcontainer; creating it (with the private `clone`,
//! 13 §6) on the first pane; trusted lifecycle commands inside the box; `task sync`;
//! stop/start/remove and teardown that never throws away unsynced work.

use super::{TaskBox, emit, sbx_root};
use crate::Server;
use crate::api::{R, err, internal, invalid};
use crate::paths;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use vk_proto::rpc::{ErrorKind, RpcError};
use vk_sandbox::config::{IsolationConfig, RepoSandbox, cache_volume, expand};
use vk_sandbox::container::{
    BOX_BIN, BOX_CREDS, BOX_HOME, BOX_INBOX, BOX_WORKSPACE, BoxMount, BoxNet, BoxSpec, BoxState,
    ContainerBox, ContainerRunner, Limits, Provider,
};
use vk_sandbox::creds::Projection;
use vk_sandbox::devcontainer::{self, DevContainer};
use vk_sandbox::net::NetworkProfile;
use vk_tasks::sync::{self, BoxRemote, SyncOutcome, SyncStatus};

/// The task's private clone inside the box (13 §6).
#[derive(Debug, Clone)]
pub struct CloneInfo {
    /// Host repo (main checkout root) that sync fetches into.
    pub repo: PathBuf,
    /// Host worktree of the task (review surface; never mounted into the box).
    pub worktree: PathBuf,
    pub branch: String,
    pub base: String,
    /// Host `<common-dir>/objects`, mounted read-only at the same path.
    pub objects: PathBuf,
    /// Host dir holding the box's repo (bind-mounted at `/workspace`).
    pub dir: PathBuf,
}

/// Everything the server keeps per container box.
pub struct CtrBox {
    pub runner: ContainerRunner,
    pub clone: Option<CloneInfo>,
    /// Trusted lifecycle commands to run once after creation (name, script).
    pub lifecycle: Vec<(String, String)>,
    pub root: PathBuf,
    pub image: String,
    pub code: &'static str,
    pub warnings: Vec<String>,
    pub devcontainer: Option<PathBuf>,
    /// Template of this box (13 §9): started from it, or to be committed after setup.
    pub template: Option<super::pool::TemplateUse>,
}

impl CtrBox {
    pub fn b(&self) -> &ContainerBox {
        &self.runner.b
    }
}

/// Code isolation for a container box: `clone` (default, tasks) or `worktree` (bind mount).
pub fn code_mode(req_code: Option<&str>, cfg: &IsolationConfig, task: bool) -> &'static str {
    match req_code.unwrap_or(cfg.container.code.as_str()) {
        "worktree" | "bind" => "worktree",
        // Ad-hoc (run-scoped) boxes have no task branch to sync: they bind the checkout.
        _ if !task => "worktree",
        _ => "clone",
    }
}

/// In-box working directory for the box.
pub fn workdir(code: &str, checkout: &Path, dc: Option<&DevContainer>) -> String {
    if code == "clone" {
        dc.and_then(|d| d.workspace_folder.clone())
            .filter(|w| !w.contains("${"))
            .unwrap_or_else(|| BOX_WORKSPACE.into())
    } else {
        checkout.to_string_lossy().into_owned()
    }
}

/// The static Linux `vibeke` for the box: config, then `$VIBEKE_ARTIFACT_DIR` /
/// `~/.cache/vibeke/releases/<version>/vibeke-linux-<arch>`, then the running binary when it
/// is itself a Linux build.
pub fn linux_vibeke(cfg: &IsolationConfig, home: &Path) -> Option<PathBuf> {
    if let Some(p) = &cfg.container.vibeke_linux {
        let p = expand(home, p);
        return p.is_file().then_some(p);
    }
    let name = format!("vibeke-linux-{}", std::env::consts::ARCH);
    let mut dirs = Vec::new();
    if let Some(d) = std::env::var_os("VIBEKE_ARTIFACT_DIR") {
        dirs.push(PathBuf::from(d));
    }
    dirs.push(home.join(".cache/vibeke/releases").join(vk_proto::VERSION));
    if let Some(p) = dirs
        .into_iter()
        .map(|d| d.join(&name))
        .find(|p| p.is_file())
    {
        return Some(p);
    }
    if cfg!(target_os = "linux") && cfg!(target_env = "musl") {
        return std::env::current_exe().ok();
    }
    None
}

/// Host dir of a box's per-pane broker sockets (reached from the box through its link).
/// `<sbx>/run` unless that would exceed the unix socket path limit (~104 bytes),
/// then a short private dir under `/tmp`.
pub fn run_dir(key: &str) -> PathBuf {
    let d = sbx_root(key).join("run");
    if d.join("egress.sock").as_os_str().len() <= 100 {
        return d;
    }
    PathBuf::from(format!("/tmp/vibeke-{}-bx", unsafe { libc::getuid() }))
        .join(vk_sandbox::runner::short_id(key))
}

/// Env values are passed literally (`--env K=V` does no expansion): entries that still reference
/// a variable (`${containerEnv:PATH}:/x` → `${PATH}:/x`) would break the box, so they are dropped.
fn literal_env(env: &[(String, String)], warnings: &mut Vec<String>) -> Vec<(String, String)> {
    env.iter()
        .filter(|(k, v)| {
            let ok = !v.contains("${");
            if !ok {
                warnings.push(format!(
                    "devcontainer env {k} references a variable and was skipped"
                ));
            }
            ok
        })
        .cloned()
        .collect()
}

/// Is a devcontainer `containerEnv`/`remoteEnv` key one the repository may not set? Loader,
/// search-path, identity, proxy, TLS-trust, git and agent/provider variables would let a
/// repository redirect the agent's traffic, credentials or executables (13 §9).
fn denied_dc_env_key(k: &str) -> bool {
    let u = k.to_ascii_uppercase();
    matches!(
        u.as_str(),
        "PATH"
            | "HOME"
            | "USER"
            | "SHELL"
            | "LD_PRELOAD"
            | "LD_LIBRARY_PATH"
            | "LD_AUDIT"
            | "SSL_CERT_FILE"
            | "SSL_CERT_DIR"
            | "NODE_OPTIONS"
            | "NODE_EXTRA_CA_CERTS"
            | "REQUESTS_CA_BUNDLE"
            | "CURL_CA_BUNDLE"
    ) || u.ends_with("_PROXY")
        || [
            "DYLD_",
            "GIT_",
            "VIBEKE_",
            "CLAUDE_",
            "CODEX_",
            "ANTHROPIC_",
            "OPENAI_",
        ]
        .iter()
        .any(|p| u.starts_with(p))
}

/// Drop devcontainer env entries the repository may not set ([`denied_dc_env_key`]) or that
/// Vibeke sets itself (`reserved`). Dropped keys (never values) are logged and reported.
fn police_dc_env(
    field: &str,
    entries: Vec<(String, String)>,
    reserved: &[&str],
    warnings: &mut Vec<String>,
) -> Vec<(String, String)> {
    let mut dropped = Vec::new();
    let kept = entries
        .into_iter()
        .filter(|(k, _)| {
            let deny = denied_dc_env_key(k) || reserved.contains(&k.as_str());
            if deny && !dropped.contains(k) {
                dropped.push(k.clone());
            }
            !deny
        })
        .collect();
    if !dropped.is_empty() {
        let names = dropped.join(", ");
        tracing::warn!(field, keys = %names, "devcontainer env keys dropped");
        warnings.push(format!(
            "devcontainer {field} keys ignored (reserved for Vibeke or security-sensitive): {names}"
        ));
    }
    kept
}

fn mkdir_private(p: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(p)?;
    std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700))
}

fn git_out(dir: &Path, args: &[&str]) -> Option<String> {
    // Hardened when the checkout is box-writable (worktree mode, restored boxes).
    let safety = vk_tasks::safety_args(dir).ok()?;
    let o = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(safety)
        .args(args)
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status
        .success()
        .then(|| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Inputs for [`build`].
pub struct BuildIn<'a> {
    pub server: &'a Arc<Server>,
    pub key: &'a str,
    pub task: Option<&'a str>,
    pub checkout: &'a Path,
    pub cfg: &'a IsolationConfig,
    pub network: NetworkProfile,
    pub image: Option<String>,
    pub code: &'static str,
    pub devcontainer: Option<String>,
    pub build_image: bool,
    pub projection: &'a Projection,
    pub home: &'a Path,
    /// Git-executed files inside the checkout (worktree mode mounts them read-only).
    pub protected: &'a [PathBuf],
}

/// Assemble the box (pure-ish: reads repo files and config, runs `git rev-parse`; never starts
/// anything except an explicitly requested, trusted devcontainer image build).
pub fn build(i: BuildIn) -> Result<CtrBox, RpcError> {
    let BuildIn {
        server,
        key,
        task,
        checkout,
        cfg,
        network,
        image,
        code,
        devcontainer: dc_rel,
        build_image,
        projection,
        home,
        protected,
    } = i;
    let unsupported = |m: String| err(ErrorKind::Unsupported, m);
    let override_rt = server.sandbox.container_runtime();
    let provider = match override_rt
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .or(cfg.container.runtime.clone())
    {
        Some(r) => Provider::from_config(&r)
            .ok_or_else(|| unsupported(format!("container runtime {r} not found")))?,
        None => vk_sandbox::container::select(None, network).ok_or_else(|| {
            unsupported(
                "no container runtime found (Apple container, OrbStack, Docker, Podman)".into(),
            )
        })?,
    };
    let mut warnings = Vec::new();
    let repo_cfg = RepoSandbox::load(checkout).unwrap_or_else(|e| {
        warnings.push(e);
        RepoSandbox::default()
    });
    let dc = devcontainer::load(
        checkout,
        dc_rel.as_deref().or(repo_cfg.devcontainer.as_deref()),
    )
    .map_err(invalid)?;
    let repo_root = vk_tasks::repo_root(checkout).map(|r| r.root);
    let dc_trusted = match (&dc, &repo_root) {
        (Some(d), Some(r)) => crate::run::devcontainer_trusted(server, r, &devcontainer::digest(d)),
        _ => false,
    };
    if let Some(d) = &dc {
        warnings.extend(d.warnings.iter().cloned());
    }
    // Image: explicit > repo sandbox.toml > devcontainer (image, or a trusted explicit build) >
    // user config.
    let mut image = image.or(repo_cfg.image.clone());
    if image.is_none()
        && let Some(d) = &dc
    {
        image = d.image.clone();
        if image.is_none() && d.build.is_some() {
            if !(build_image || cfg.container.build) {
                return Err(invalid(format!(
                    "{} builds its image from a Dockerfile; pass --build (or [isolation.container] build = true) after reviewing it, or --image",
                    d.path.display()
                )));
            }
            if !dc_trusted {
                return Err(err(
                    ErrorKind::PermissionDenied,
                    format!(
                        "building the devcontainer image needs repo trust: review {} then run `vibeke policy trust {}`",
                        d.path.display(),
                        repo_root.as_deref().unwrap_or(checkout).display()
                    ),
                ));
            }
            let argv = d.build_argv(provider.cli()).unwrap_or_default();
            let out = vk_sandbox::container::run_cmd(
                &argv,
                &vk_sandbox::container::cli_env(&server.opts.env),
                None,
                Duration::from_secs(1800),
            )
            .map_err(internal)?;
            if !out.ok {
                return Err(unsupported(format!(
                    "devcontainer build failed: {}",
                    out.stderr.trim()
                )));
            }
            image = d.build_tag();
        }
    }
    let image = image.or(cfg.container.image.clone()).ok_or_else(|| {
        invalid("container isolation needs an image (--image, .vibeke/sandbox.toml, a devcontainer, or [isolation.container] image)")
    })?;
    let root = sbx_root(key);
    let run_dir = run_dir(key);
    let creds = root.join("shared");
    if let Some(parent) = run_dir.parent().filter(|p| p.starts_with("/tmp")) {
        mkdir_private(parent).map_err(internal)?;
    }
    for d in [&root, &run_dir, &creds] {
        mkdir_private(d).map_err(internal)?;
    }
    let wd = workdir(code, checkout, dc.as_ref());
    let user = dc.as_ref().and_then(|d| d.user().map(str::to_string));
    let host_ids = format!("{}:{}", unsafe { libc::getuid() }, unsafe {
        libc::getgid()
    });
    let mut mounts = vec![
        BoxMount::Bind {
            host: paths::Paths::inbox(),
            target: BOX_INBOX.into(),
            read_only: true,
        },
        BoxMount::Bind {
            host: creds.clone(),
            target: BOX_CREDS.into(),
            read_only: false,
        },
    ];
    let _ = std::fs::create_dir_all(paths::Paths::inbox());
    let mut env: Vec<(String, String)> = vec![
        ("VIBEKE_ISOLATION".into(), "container".into()),
        ("VIBEKE_NETWORK".into(), network.as_str().into()),
    ];
    if user.is_none() {
        let h = root.join("home");
        mkdir_private(&h).map_err(internal)?;
        mounts.push(BoxMount::Bind {
            host: h,
            target: BOX_HOME.into(),
            read_only: false,
        });
        env.push(("HOME".into(), BOX_HOME.into()));
    }
    let mut clone = None;
    if code == "clone" {
        // The objects dir of the detected layout is mounted into the box: refuse a `.git`
        // file that points at some other host repository (13 §6).
        let layout = vk_sandbox::GitLayout::detect_checked(
            checkout,
            &super::trusted_git_roots(server, task),
        )
        .map_err(|e| err(ErrorKind::PermissionDenied, e))?
        .ok_or_else(|| invalid(format!("{} is not a git checkout", checkout.display())))?;
        let branch = layout
            .branch
            .clone()
            .filter(|b| b != "HEAD")
            .ok_or_else(|| invalid("clone isolation needs a task branch (detached HEAD)"))?;
        let base = git_out(checkout, &["rev-parse", "HEAD"])
            .ok_or_else(|| invalid("cannot resolve the checkout's HEAD"))?;
        let objects = layout.common_dir.join("objects");
        let dir = root.join("workspace");
        mkdir_private(&dir).map_err(internal)?;
        mounts.push(BoxMount::Bind {
            host: dir.clone(),
            target: wd.clone(),
            read_only: false,
        });
        mounts.push(BoxMount::Bind {
            host: objects.clone(),
            target: objects.to_string_lossy().into_owned(),
            read_only: true,
        });
        clone = Some(CloneInfo {
            repo: repo_root
                .clone()
                .unwrap_or_else(|| layout.common_dir.parent().unwrap_or(checkout).to_path_buf()),
            worktree: checkout.to_path_buf(),
            branch,
            base,
            objects,
            dir,
        });
    } else {
        mounts.push(BoxMount::Bind {
            host: checkout.to_path_buf(),
            target: wd.clone(),
            read_only: false,
        });
        // Host git trusts `<checkout>/.git`: never writable from the box (13 §6). A regular
        // checkout's `.git` dir and a linked worktree's `.git` file are both re-mounted
        // read-only on top (a mount point can't be replaced or renamed), as is every file
        // host git would execute or read as config from inside the checkout.
        let dot = checkout.join(".git");
        if std::fs::symlink_metadata(&dot).is_ok() {
            mounts.push(BoxMount::Bind {
                host: dot.clone(),
                target: Path::new(&wd).join(".git").to_string_lossy().into_owned(),
                read_only: true,
            });
        }
        for p in protected {
            if p.exists()
                && let Ok(rel) = p.strip_prefix(checkout)
            {
                mounts.push(BoxMount::Bind {
                    host: p.clone(),
                    target: Path::new(&wd).join(rel).to_string_lossy().into_owned(),
                    read_only: true,
                });
            }
        }
        warnings.push(
            "worktree mode: the host checkout is bind-mounted with its .git read-only; a linked worktree's .git file points at the host repo, which is not mounted, so git does not work inside the box".into(),
        );
    }
    let bin = linux_vibeke(cfg, home);
    if let Some(b) = &bin {
        mounts.push(BoxMount::Bind {
            host: b.clone(),
            target: BOX_BIN.into(),
            read_only: true,
        });
        env.push(("VIBEKE_BIN".into(), BOX_BIN.into()));
        // Hook configs copied from the host name the host binary's absolute path.
        let host_bin = &server.opts.bin;
        if host_bin.is_absolute() && host_bin.is_file() && !host_bin.starts_with("/bin") {
            mounts.push(BoxMount::Bind {
                host: b.clone(),
                target: host_bin.to_string_lossy().into_owned(),
                read_only: true,
            });
        }
    }
    for c in &cfg.container.caches {
        match cache_volume(c) {
            Some((vol, target, var)) => {
                mounts.push(BoxMount::Volume {
                    name: vol.into(),
                    target: target.into(),
                });
                env.push((var.into(), target.into()));
            }
            None => warnings.push(format!("unknown cache volume {c}")),
        }
    }
    // Every key Vibeke itself sets in the box env: repo-controlled env never overrides them.
    let vibeke_env_keys: Vec<String> = env.iter().map(|(k, _)| k.clone()).collect();
    if let Some(d) = &dc {
        // Repo-controlled: plain per-task volume names, binds only from inside a trusted
        // checkout in worktree mode, never sockets or a writable view of `.git` (13 §9).
        let ns = format!("vk-{}", vk_sandbox::runner::short_id(key));
        let (m, w) = devcontainer::police_mounts(
            &d.mounts,
            &devcontainer::MountPolicy {
                checkout,
                trusted: dc_trusted,
                worktree: code == "worktree",
                namespace: &ns,
                protected,
            },
        );
        mounts.extend(m);
        warnings.extend(w);
        // Repo-controlled env goes first and never overrides what Vibeke sets.
        let reserved: Vec<&str> = vibeke_env_keys.iter().map(String::as_str).collect();
        let mut dc_env = police_dc_env(
            "containerEnv",
            literal_env(&d.container_env, &mut warnings),
            &reserved,
            &mut warnings,
        );
        dc_env.append(&mut env);
        env = dc_env;
    }
    // Credentials: path-valued projection env is rewritten to the box mount; everything else is
    // a secret passed by name per exec (13 §8).
    let creds_prefix = creds.to_string_lossy().into_owned();
    // The projected homes are writable (sessions, caches); the credential files in them are
    // re-mounted read-only on top, so the box can neither modify nor replace them.
    for f in &projection.read_only_files {
        if let Ok(rel) = f.strip_prefix(&creds) {
            mounts.push(BoxMount::Bind {
                host: f.clone(),
                target: Path::new(BOX_CREDS)
                    .join(rel)
                    .to_string_lossy()
                    .into_owned(),
                read_only: true,
            });
        }
    }
    let mut exec_env = Vec::new();
    let mut secrets = Vec::new();
    for (k, v) in &projection.env {
        match v.strip_prefix(&creds_prefix) {
            Some(rest) => exec_env.push((k.clone(), format!("{BOX_CREDS}{rest}"))),
            None => secrets.push((k.clone(), v.clone())),
        }
    }
    if let Some(d) = &dc {
        // remoteEnv also goes first: the projected credential env and the per-pane identity
        // (`VIBEKE_*`, set over it in `pane_exec`) always win.
        let reserved: Vec<&str> = exec_env
            .iter()
            .chain(secrets.iter())
            .map(|(k, _)| k.as_str())
            .chain(vibeke_env_keys.iter().map(String::as_str))
            .collect();
        let mut dc_env = police_dc_env(
            "remoteEnv",
            literal_env(&d.remote_env, &mut warnings),
            &reserved,
            &mut warnings,
        );
        dc_env.append(&mut exec_env);
        exec_env = dc_env;
    }
    let limits = Limits {
        cpus: repo_cfg.cpus.clone().or(cfg.container.cpus.clone()),
        memory: repo_cfg.memory.clone().or(cfg.container.memory.clone()),
        pids: cfg.container.pids,
    };
    let mut spec = BoxSpec {
        provider,
        name: format!("vk-{}", vk_sandbox::runner::short_id(key)),
        image: image.clone(),
        labels: vec![
            ("vibeke.box".into(), "1".into()),
            ("vibeke.session".into(), server.opts.session.clone()),
            ("vibeke.key".into(), key.to_string()),
        ],
        net: BoxNet::for_profile(network),
        workdir: wd,
        mounts,
        env,
        user: Some(user.clone().unwrap_or(host_ids)),
        limits,
        in_box_vibeke: bin.is_some(),
        cap_add: cfg.container.cap_add.clone(),
    };
    // Lifecycle (trusted only, 09 §4): devcontainer commands, then `.vibeke/setup.sh`.
    let mut lifecycle = Vec::new();
    if let Some(d) = &dc {
        if dc_trusted {
            lifecycle.extend(d.lifecycle.iter().map(|(n, c)| (n.clone(), c.script())));
        } else if !d.lifecycle.is_empty() {
            warnings.push(format!(
                "devcontainer lifecycle commands skipped: repo not trusted (review {} then `vibeke policy trust`)",
                d.path.display()
            ));
        }
    }
    if task.is_some() && checkout.join(".vibeke/setup.sh").is_file() {
        let trusted = repo_root.as_deref().is_some_and(|r| {
            crate::run::vibeke_dir_digest(checkout)
                .is_some_and(|dg| crate::run::repo_trusted(server, r, &dg))
        });
        if trusted {
            lifecycle.push(("setup".into(), "sh .vibeke/setup.sh".into()));
        } else {
            warnings.push("`.vibeke/setup.sh` skipped: repo not trusted".into());
        }
    }
    // Template (13 §9): a committed image of the same setup replaces image + steps.
    let cli_env = vk_sandbox::container::cli_env(&server.opts.env);
    let mut image = image;
    let template = super::pool::apply_template(
        server,
        cfg,
        &spec.provider,
        &cli_env,
        &mut image,
        &mut lifecycle,
    );
    if template.as_ref().is_some_and(|t| t.from_template) {
        spec.image = image.clone();
        warnings.push(format!(
            "started from template {image}; setup steps were cached in it"
        ));
    }
    let b = ContainerBox {
        spec,
        cli_env,
        secrets,
        exec_env,
        run_dir,
        visible_roots: if code == "clone" {
            vec![]
        } else {
            vec![checkout.to_string_lossy().into_owned()]
        },
        shell: cfg.container.shell.clone(),
    };
    Ok(CtrBox {
        runner: ContainerRunner { b },
        clone,
        lifecycle,
        root,
        image,
        code,
        warnings,
        devcontainer: dc.map(|d| d.path),
        template,
    })
}

/// Create/start the box; on creation, make the private clone and kick off lifecycle commands
/// in the background. Blocking: call from `spawn_blocking`.
pub fn ensure(
    server: &Arc<Server>,
    key: &str,
    task: Option<&str>,
    c: &CtrBox,
) -> Result<bool, RpcError> {
    let created = c
        .b()
        .ensure_running()
        .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
    if let Some(cl) = &c.clone {
        let name = git_out(&cl.worktree, &["config", "user.name"]);
        let email = git_out(&cl.worktree, &["config", "user.email"]);
        let script = sync::box_clone_script(
            &cl.objects,
            &c.b().spec.workdir,
            &cl.branch,
            &cl.base,
            name.as_deref(),
            email.as_deref(),
        );
        let out = c
            .b()
            .exec_script(&script, None, Duration::from_secs(300))
            .map_err(internal)?;
        if !out.ok {
            if created {
                let _ = c.b().remove();
            }
            return Err(err(
                ErrorKind::Unsupported,
                format!(
                    "could not create the box's private clone (does the image have git?): {}",
                    out.stderr.trim()
                ),
            ));
        }
    }
    if created {
        emit(
            server,
            "sandbox.started",
            json!({"task": task, "sandbox": key}),
            json!({"container": c.b().spec.name, "image": c.image, "created": true}),
        );
        if !c.lifecycle.is_empty() {
            spawn_lifecycle(server, key, task, c, c.lifecycle.clone());
        }
    }
    Ok(created)
}

/// Run lifecycle steps in the box on a thread (log in the box root, then
/// `sandbox.setup_finished`). Every step is an `exec` into the container: nothing runs on the
/// host. With a pending template (13 §9), a successful run is committed as that template.
pub fn spawn_lifecycle(
    server: &Arc<Server>,
    key: &str,
    task: Option<&str>,
    c: &CtrBox,
    steps: Vec<(String, String)>,
) {
    let srv = server.clone();
    let (b, log) = (c.b().clone(), c.root.join("setup.log"));
    let (key, task) = (key.to_string(), task.map(str::to_string));
    let commit = c
        .template
        .as_ref()
        .filter(|t| !t.from_template && steps == c.lifecycle)
        .map(|t| t.key.clone());
    std::thread::spawn(move || {
        let mut status = "ok".to_string();
        let mut text = String::new();
        for (name, script) in steps {
            text.push_str(&format!("$ {name}: {script}\n"));
            match b.exec_script(&script, None, Duration::from_secs(1800)) {
                Ok(o) => {
                    text.push_str(&o.stdout);
                    text.push_str(&o.stderr);
                    if !o.ok {
                        status = format!("failed: {name}");
                        break;
                    }
                }
                Err(e) => {
                    status = format!("failed: {name}: {e}");
                    break;
                }
            }
        }
        let _ = std::fs::write(&log, text);
        emit(
            &srv,
            "sandbox.setup_finished",
            json!({"task": task, "sandbox": key}),
            json!({"status": status, "log": log}),
        );
        if status == "ok"
            && let Some(t) = commit
        {
            super::pool::after_setup(&srv, &key, task.as_deref(), &b, &t);
        }
    });
}

/// `task.setup` for a container task (blocking): the steps run **in the box** through the
/// container runner, never on the host. They are the box's trusted lifecycle commands plus
/// `.vibeke/setup.sh`, whose trust is checked again now (so a rerun after `policy trust`
/// picks it up). Starts the box if it is stopped.
pub fn rerun_setup(server: &Arc<Server>, tb: &TaskBox, c: &CtrBox) -> Result<Value, RpcError> {
    let mut steps: Vec<(String, String)> = c
        .lifecycle
        .iter()
        .filter(|(n, _)| n != "setup")
        .cloned()
        .collect();
    let mut warnings = Vec::new();
    if tb.checkout.join(".vibeke/setup.sh").is_file() {
        let trusted = vk_tasks::repo_root(&tb.checkout).is_some_and(|r| {
            crate::run::vibeke_dir_digest(&tb.checkout)
                .is_some_and(|dg| crate::run::repo_trusted(server, &r.root, &dg))
        });
        if trusted {
            steps.push(("setup".into(), "sh .vibeke/setup.sh".into()));
        } else {
            warnings.push("`.vibeke/setup.sh` skipped: repo not trusted".to_string());
        }
    }
    let created = ensure(server, &tb.key, tb.task.as_deref(), c)?;
    // A box created just now already ran its create-time lifecycle in `ensure`.
    let started = !created && !steps.is_empty();
    if started {
        spawn_lifecycle(server, &tb.key, tb.task.as_deref(), c, steps.clone());
    }
    Ok(json!({
        "started": started || (created && !c.lifecycle.is_empty()),
        "container": c.b().spec.name,
        "commands": steps.iter().map(|(n, s)| json!({"source": n, "command": s})).collect::<Vec<_>>(),
        "log": c.root.join("setup.log"),
        "warnings": warnings,
    }))
}

fn remote_for(c: &CtrBox, cl: &CloneInfo) -> (BoxRemote, bool) {
    if c.b().state() == BoxState::Running {
        (
            BoxRemote {
                url: c.b().spec.workdir.clone(),
                upload_pack: c.b().git_service("upload-pack"),
                receive_pack: c.b().git_service("receive-pack"),
                local: None,
            },
            true,
        )
    } else {
        // Stopped box: the same repo on the host. Pulls run a hardened `upload-pack`; pushes
        // only write the side ref (no `receive-pack` on the host, `vk_tasks::sync`).
        (BoxRemote::local(&cl.dir), false)
    }
}

/// `task.sync` (blocking).
pub fn sync_task(c: &CtrBox, direction: &str, force: bool) -> Result<Vec<SyncOutcome>, RpcError> {
    let cl = c
        .clone
        .as_ref()
        .ok_or_else(|| invalid("this task's box has no private clone (code isolation `clone`)"))?;
    let (remote, running) = remote_for(c, cl);
    let ns = vk_sandbox::runner::short_id(&c.b().spec.name);
    let mut out = Vec::new();
    if matches!(direction, "push" | "both") {
        let o = sync::sync_push(&cl.repo, &remote, &cl.branch, &cl.branch)
            .map_err(|e| err(ErrorKind::Conflict, e.to_string()))?;
        out.push(o);
        if running {
            let ff = c
                .b()
                .exec_script(
                    &sync::box_ff_script(&c.b().spec.workdir, &cl.branch),
                    None,
                    Duration::from_secs(120),
                )
                .map_err(internal)?;
            if !ff.ok {
                tracing::info!(stderr = %ff.stderr.trim(), "box did not fast-forward after push");
            }
        }
    }
    if matches!(direction, "pull" | "both") {
        let o = sync::sync_pull(&cl.repo, &remote, &cl.branch, &cl.branch, &ns, force)
            .map_err(|e| err(ErrorKind::Conflict, e.to_string()))?;
        out.push(o);
    }
    if out.is_empty() {
        return Err(invalid("direction is pull | push | both"));
    }
    Ok(out)
}

/// What happened to a box at task end (blocking): pull unsynced work first, then **stop** the
/// box (so nothing can write while it is checked) and inspect its repo from the host
/// ([`sync::box_leftovers`]: index, worktree, untracked files, other branches, detached HEAD,
/// stashes, commits made after the pull). Remove only when everything is on the host, else
/// keep it stopped.
pub fn finish(server: &Server, tb: &TaskBox, c: &CtrBox, policy: &str) -> Value {
    let mut synced = Value::Null;
    let mut safe = c.clone.is_none();
    if c.clone.is_some() && c.b().state() != BoxState::Missing {
        match sync_task(c, "pull", false) {
            Ok(o) => {
                safe = o
                    .iter()
                    .all(|x| matches!(x.status, SyncStatus::UpToDate | SyncStatus::FastForwarded));
                synced = serde_json::to_value(&o).unwrap_or_default();
                record_sync(server, tb, &o);
            }
            Err(e) => synced = json!({"error": e.message}),
        }
    }
    let mut leftovers: Vec<String> = Vec::new();
    let mut res = Ok(());
    let action = match policy {
        "keep" => "kept",
        "stop" => {
            res = c.b().stop();
            "stopped"
        }
        _ => {
            // Stop first: writes racing the check are impossible once the box is down.
            res = c.b().stop();
            if safe
                && res.is_ok()
                && let Some(cl) = &c.clone
            {
                leftovers = sync::box_leftovers(&cl.dir, &cl.repo, &cl.branch);
                safe = leftovers.is_empty();
            }
            if safe && res.is_ok() {
                res = c.b().remove();
                "removed"
            } else {
                "stopped"
            }
        }
    };
    if action == "removed" && res.is_ok() {
        let _ = std::fs::remove_dir_all(&c.root);
        let _ = std::fs::remove_dir_all(&c.runner.b.run_dir);
    }
    json!({"container": c.b().spec.name, "action": action, "sync": synced, "leftovers": leftovers, "error": res.err().map(|e| e.to_string()), "unsynced_kept": action != "removed" && !safe})
}

pub fn record_sync(server: &Server, tb: &TaskBox, o: &[SyncOutcome]) {
    for x in o {
        emit(
            server,
            "task.synced",
            json!({"task": tb.task, "sandbox": tb.key}),
            json!({"direction": x.direction, "status": x.status, "commits": x.commits, "from": x.from, "to": x.to, "ref": x.reference}),
        );
    }
}

// ---- the box link (egress + brokers over `exec -i` stdio, vk_remote::boxlink) ------------------

/// One box's link: the current mux (while the in-box end runs) and the panes whose broker
/// sockets the box should serve. Listen channels are reopened after every reconnect.
#[derive(Default)]
pub struct BoxLink {
    mux: std::sync::Mutex<Option<vk_remote::Mux>>,
    panes: std::sync::Mutex<std::collections::HashMap<String, Option<tokio::io::DuplexStream>>>,
    pub connected: std::sync::atomic::AtomicBool,
}

impl BoxLink {
    fn open_listen(self: &Arc<Self>, m: vk_remote::Mux, short: String) {
        let me = self.clone();
        tokio::spawn(async move {
            match m.open(&format!("listen:{short}")).await {
                Ok(ctl) => {
                    me.panes.lock().unwrap().insert(short, Some(ctl));
                }
                Err(e) => tracing::warn!(error = %e, "box link: broker listen failed"),
            }
        });
    }
    /// Serve `pane`'s broker inside the box (now, and after every reconnect).
    pub fn add_pane(self: &Arc<Self>, pane: &str) {
        let short = vk_sandbox::runner::short_id(pane);
        self.panes.lock().unwrap().insert(short.clone(), None);
        if let Some(m) = self.mux.lock().unwrap().clone() {
            self.open_listen(m, short);
        }
    }
    /// Use `m` as the link's mux (tests: an in-process box side instead of `exec -i`).
    #[cfg(test)]
    pub fn attach_for_test(&self, m: vk_remote::Mux) {
        *self.mux.lock().unwrap() = Some(m);
        self.connected
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    /// A connection to `127.0.0.1:<port>` inside the box (`tcp:` channel; box previews).
    pub async fn open_tcp(&self, port: u16) -> anyhow::Result<tokio::io::DuplexStream> {
        let m = self
            .mux
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("the box link is down"))?;
        m.open(&format!("tcp:{port}")).await
    }
    pub fn remove_pane(&self, pane: &str) {
        self.panes
            .lock()
            .unwrap()
            .remove(&vk_sandbox::runner::short_id(pane));
    }
}

/// Keep the link to a running box up: `<runtime> exec -i <box> vibeke sandbox bridge`, the host
/// end bridging `egress` to the proxy and `broker:<pane>` to the host broker sockets. Waits while
/// the box is stopped; reconnects after the exec ends. Abort the handle to stop.
pub fn start_link(
    b: ContainerBox,
    proxy_port: Option<u16>,
) -> (Arc<BoxLink>, tokio::task::JoinHandle<()>) {
    let link = Arc::new(BoxLink::default());
    let l2 = link.clone();
    let h = tokio::spawn(async move {
        loop {
            let b2 = b.clone();
            let running = tokio::task::spawn_blocking(move || b2.state())
                .await
                .map(|s| s == BoxState::Running)
                .unwrap_or(false);
            if running {
                let argv = b.link_argv();
                let child = tokio::process::Command::new(&argv[0])
                    .args(&argv[1..])
                    .env_clear()
                    .envs(b.cli_env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
                    .stdin(std::process::Stdio::piped())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .kill_on_drop(true)
                    .spawn();
                if let Ok(mut child) = child
                    && let (Some(si), Some(so)) = (child.stdin.take(), child.stdout.take())
                {
                    let m = vk_remote::Mux::start(
                        so,
                        si,
                        "bridge",
                        Some(vk_remote::boxlink::host_acceptor(
                            proxy_port,
                            b.run_dir.clone(),
                        )),
                    );
                    *l2.mux.lock().unwrap() = Some(m.clone());
                    l2.connected
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    let shorts: Vec<String> = l2.panes.lock().unwrap().keys().cloned().collect();
                    for s in shorts {
                        l2.open_listen(m.clone(), s);
                    }
                    tokio::select! {
                        _ = m.closed() => {}
                        _ = child.wait() => {}
                    }
                    *l2.mux.lock().unwrap() = None;
                    l2.connected
                        .store(false, std::sync::atomic::Ordering::SeqCst);
                    for v in l2.panes.lock().unwrap().values_mut() {
                        *v = None;
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
    (link, h)
}

/// Container-specific fields for `sandbox.list` (blocking: inspects the box).
pub fn describe(c: &CtrBox) -> Value {
    json!({
        "container": c.b().spec.name,
        "state": c.b().state().as_str(),
        "image": c.image,
        "code": c.code,
        "workdir": c.b().spec.workdir,
        "clone": c.clone.as_ref().map(|cl| json!({"branch": cl.branch, "base": cl.base})),
        "devcontainer": c.devcontainer,
        "warnings": c.warnings,
        "in_box_vibeke": c.b().spec.in_box_vibeke,
    })
}

/// `sandbox.start|stop|remove` and `task.sync`.
pub async fn api(server: &Arc<Server>, method: &str, p: &Value) -> R {
    let t = crate::api::req(p, "task")?;
    let key = server
        .with_core(|c| c.task(t).map(|x| x.id.clone()))
        .unwrap_or_else(|| t.to_string());
    let tb = server
        .sandbox
        .get(&key)
        .ok_or_else(|| crate::api::not_found("sandbox", t))?;
    if !matches!(tb.runner, super::BoxRunner::Container(_)) {
        return Err(invalid("not a container box"));
    }
    let srv = server.clone();
    let method = method.to_string();
    let p = p.clone();
    tokio::task::spawn_blocking(move || {
        let super::BoxRunner::Container(c) = &tb.runner else {
            unreachable!()
        };
        let subject = json!({"task": tb.task, "sandbox": tb.key});
        match method.as_str() {
            "sandbox.start" => {
                let created = ensure(&srv, &tb.key, tb.task.as_deref(), c)?;
                emit(
                    &srv,
                    "sandbox.resumed",
                    subject,
                    json!({"created": created}),
                );
                Ok(json!({"state": c.b().state().as_str(), "created": created}))
            }
            "sandbox.stop" => {
                c.b()
                    .stop()
                    .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
                emit(&srv, "sandbox.suspended", subject, json!({}));
                Ok(json!({"state": c.b().state().as_str()}))
            }
            "sandbox.remove" => {
                let force = p.get("force").and_then(Value::as_bool).unwrap_or(false);
                let r = finish(&srv, &tb, c, if force { "remove-force" } else { "remove" });
                if force && r["action"] != "removed" {
                    c.b()
                        .remove()
                        .map_err(|e| err(ErrorKind::Unsupported, e.to_string()))?;
                    let _ = std::fs::remove_dir_all(&c.root);
                    let _ = std::fs::remove_dir_all(&c.runner.b.run_dir);
                }
                emit(&srv, "sandbox.destroyed", subject, r.clone());
                Ok(r)
            }
            "task.sync" => {
                let dir = p
                    .get("direction")
                    .and_then(Value::as_str)
                    .unwrap_or("pull")
                    .to_string();
                let force = p.get("force").and_then(Value::as_bool).unwrap_or(false);
                let o = sync_task(c, &dir, force)?;
                record_sync(&srv, &tb, &o);
                Ok(json!({"task": tb.task, "synced": o}))
            }
            _ => Err(invalid("unknown method")),
        }
    })
    .await
    .map_err(internal)?
}

#[cfg(test)]
mod dc_env_tests {
    use super::*;

    fn kv(k: &str) -> (String, String) {
        (k.to_string(), "x".to_string())
    }

    #[test]
    fn devcontainer_env_cannot_override_sensitive_or_vibeke_keys() {
        let entries: Vec<(String, String)> = [
            "PATH",
            "HOME",
            "LD_PRELOAD",
            "DYLD_INSERT_LIBRARIES",
            "https_proxy",
            "NO_PROXY",
            "SSL_CERT_FILE",
            "NODE_OPTIONS",
            "GIT_SSH_COMMAND",
            "VIBEKE_SOCKET",
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_BASE_URL",
            "OPENAI_API_KEY",
            "CODEX_HOME",
            "CARGO_TARGET_DIR",
            "RUST_LOG",
            "MY_TOKEN_PATH",
        ]
        .into_iter()
        .map(kv)
        .collect();
        let mut warnings = Vec::new();
        let kept = police_dc_env(
            "containerEnv",
            entries,
            &["CARGO_TARGET_DIR"],
            &mut warnings,
        );
        let keys: Vec<&str> = kept.iter().map(|(k, _)| k.as_str()).collect();
        // Vibeke's own key (a cache volume var) and every sensitive key are dropped.
        assert_eq!(keys, ["RUST_LOG", "MY_TOKEN_PATH"]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("LD_PRELOAD") && warnings[0].contains("CARGO_TARGET_DIR"));
        // Values never appear in the warning.
        assert!(!warnings[0].contains("=x"));
        // Nothing to drop: no warning.
        let mut w2 = Vec::new();
        assert_eq!(
            police_dc_env("remoteEnv", vec![kv("FOO")], &[], &mut w2).len(),
            1
        );
        assert!(w2.is_empty());
    }
}
