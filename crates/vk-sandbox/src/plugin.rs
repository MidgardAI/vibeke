//! Restricted legacy mode for Herdr plugins (07 §7.7, 09 §6): a plugin's argv runs under the
//! `sandbox` level (13) with the existing Seatbelt (macOS) and bubblewrap + Landlock + seccomp
//! (Linux) policies.
//!
//! What the plugin can touch:
//!
//! * its own directory and its config directory: **read-only**;
//! * its own state directory (and a private tmp/cache dir inside it): **read-write**;
//! * the system (read-only), the home allowlist of [`crate::policy::DEFAULT_HOME_READ`], and
//!   nothing else of the user's home, nor Vibeke's runtime, state or config (other plugins'
//!   state, brokers and the registry stay hidden);
//! * the network: **off** unless the plugin was granted it, and then remote IP endpoints only;
//! * Unix sockets: only the invocation's own broker (plus the system DNS resolver socket on
//!   macOS when the network is granted), whatever the network setting. Vibeke's runtime dir,
//!   including the broker registry and the other brokers, stays hidden; the server also binds
//!   every broker to its invocation (peer process or per-invocation token), so a socket path
//!   alone is not an identity.
//!
//! The environment is the scrubbed allowlist of [`crate::env`], not the user's. Where no working
//! sandbox exists [`probe`] fails and the caller refuses to run the plugin: there is no silent
//! fallback to host execution.

use crate::env;
use crate::policy::{NetMode, Policy, SandboxSpec};
use std::path::{Path, PathBuf};

/// Test hook: force [`probe`] to fail (only honoured when `VIBEKE_TEST_HOOKS=1`).
pub const FORCE_UNAVAILABLE_ENV: &str = "VIBEKE_TEST_PLUGIN_SANDBOX_UNAVAILABLE";

/// Everything one sandboxed plugin invocation needs.
#[derive(Debug, Clone)]
pub struct PluginBox {
    pub home: PathBuf,
    /// The plugin's directory (read-only).
    pub plugin_root: PathBuf,
    /// Plugin-owned config dir (read-only).
    pub config_dir: PathBuf,
    /// Plugin-owned state dir (read-write); its private tmp/cache live below it.
    pub state_dir: PathBuf,
    /// Vibeke's own directories to hide (state root, runtime root, config dir).
    pub hidden: Vec<PathBuf>,
    /// Read-only extras: the `herdr` launcher directory and the `vibeke` binary. Never the
    /// broker registry (`brokers.json`): it names every other invocation's broker.
    pub extra_read: Vec<PathBuf>,
    /// The invocation's own broker socket(s).
    pub sockets: Vec<PathBuf>,
    /// Outbound network (default false).
    pub network: bool,
    /// `vibeke` binary (the Linux outer helper runs it).
    pub vibeke_bin: Option<PathBuf>,
    /// Where the generated profile/policy is written: outside every path the plugin can
    /// write, so a concurrent invocation of the same plugin cannot swap it.
    pub profile_dir: PathBuf,
}

/// The wrapped command for one invocation.
#[derive(Debug, Clone)]
pub struct Prepared {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    /// The generated profile (remove it once the process was started or failed).
    pub profile: PathBuf,
    pub policy: Policy,
}

fn unavailable(reason: impl Into<String>) -> String {
    format!(
        "restricted (sandboxed) plugin mode is not available here: {}; \
         set isolate = \"host\" for this plugin or run it where the sandbox works",
        reason.into()
    )
}

/// Can plugins be sandboxed on this host? Checks the tool exists and that a trivial command
/// actually runs under a minimal profile (a nested sandbox or a missing kernel feature fails
/// here, not in the middle of an action).
pub fn probe() -> Result<(), String> {
    if std::env::var("VIBEKE_TEST_HOOKS").as_deref() == Ok("1")
        && std::env::var_os(FORCE_UNAVAILABLE_ENV).is_some()
    {
        return Err(unavailable("disabled by a test hook"));
    }
    static CACHE: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    CACHE.get_or_init(probe_uncached).clone()
}

#[cfg(target_os = "macos")]
fn probe_uncached() -> Result<(), String> {
    use std::process::{Command, Stdio};
    if !crate::seatbelt::available() {
        return Err(unavailable(format!(
            "{} not found",
            crate::seatbelt::SANDBOX_EXEC
        )));
    }
    let profile = "(version 1)\n(deny default)\n(allow process-exec)\n(allow process-fork)\n(allow file-read*)\n";
    let dir = std::env::temp_dir().join(format!("vk-plugin-sbx-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| unavailable(e.to_string()))?;
    let file = dir.join("p.sb");
    std::fs::write(&file, profile).map_err(|e| unavailable(e.to_string()))?;
    let out = Command::new(crate::seatbelt::SANDBOX_EXEC)
        .arg("-f")
        .arg(&file)
        .arg("/usr/bin/true")
        .stdin(Stdio::null())
        .output();
    let _ = std::fs::remove_dir_all(&dir);
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(unavailable(format!(
            "sandbox-exec cannot apply a profile here ({})",
            String::from_utf8_lossy(&o.stderr).trim()
        ))),
        Err(e) => Err(unavailable(format!("sandbox-exec: {e}"))),
    }
}

#[cfg(target_os = "linux")]
fn probe_uncached() -> Result<(), String> {
    use std::process::{Command, Stdio};
    if !crate::linux::bwrap_available() {
        return Err(unavailable("bubblewrap (bwrap) is not installed"));
    }
    let out = Command::new(crate::linux::BWRAP)
        .args([
            "--unshare-user-try",
            "--unshare-net",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "true",
        ])
        .stdin(Stdio::null())
        .output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(unavailable(format!(
            "bubblewrap cannot create a sandbox here ({})",
            String::from_utf8_lossy(&o.stderr).trim()
        ))),
        Err(e) => Err(unavailable(format!("bwrap: {e}"))),
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn probe_uncached() -> Result<(), String> {
    Err(unavailable("this platform has no sandbox backend"))
}

impl PluginBox {
    /// The sandbox rules for this plugin.
    pub fn spec(&self) -> SandboxSpec {
        let mut extra_read = vec![self.plugin_root.clone(), self.config_dir.clone()];
        extra_read.extend(self.extra_read.iter().cloned());
        SandboxSpec {
            home: self.home.clone(),
            // The state dir plays the part of the writable checkout: writable, and re-allowed
            // for reads under Vibeke's hidden state root.
            checkout: self.state_dir.clone(),
            git: None,
            private_dir: self.state_dir.join(".sandbox"),
            extra_read,
            extra_write: vec![],
            read_only_files: vec![],
            hidden: self.hidden.clone(),
            home_read: None,
            unix_sockets: self.sockets.clone(),
            network: if self.network {
                NetMode::Open
            } else {
                NetMode::None
            },
            allow_bind_localhost: false,
            protected: vec![],
        }
    }

    /// Wrap `argv` (resolved, absolute `argv[0]` for plugin-relative programs) for execution
    /// in `cwd` (the plugin root). `inherited` is the invocation environment *before*
    /// scrubbing; Herdr's own variables are kept, everything else must be allowlisted.
    pub fn prepare(
        &self,
        argv: &[String],
        cwd: &Path,
        inv_env: Vec<(String, String)>,
        id: &str,
    ) -> Result<Prepared, String> {
        probe()?;
        std::fs::create_dir_all(&self.state_dir).map_err(|e| e.to_string())?;
        let private = crate::fsafe::ensure_dir_under(&self.state_dir, Path::new(".sandbox"))
            .map_err(|e| format!("sandbox dir: {e}"))?;
        let policy = Policy::from_spec(&self.spec());
        let _ = std::fs::create_dir_all(&self.profile_dir);
        // A fresh file per invocation; the plugin cannot write here.
        let profile = self.profile_dir.join(format!("{id}.{}", {
            if cfg!(target_os = "linux") {
                "policy.json"
            } else {
                "sb"
            }
        }));
        let bytes = if cfg!(target_os = "linux") {
            serde_json::to_vec_pretty(&policy).map_err(|e| e.to_string())?
        } else {
            crate::seatbelt::render(&policy).into_bytes()
        };
        let _ = std::fs::remove_file(&profile);
        crate::fsafe::write_nofollow(&profile, &bytes, 0o600)
            .map_err(|e| format!("profile: {e}"))?;
        let mut env = inv_env;
        env::private_dirs(&mut env, &private).map_err(|e| format!("sandbox dirs: {e}"))?;
        env::set(&mut env, "VIBEKE_ISOLATION", "sandbox");
        env::set(
            &mut env,
            "VIBEKE_NETWORK",
            if self.network { "open" } else { "none" },
        );
        let wrapped = if cfg!(target_os = "linux") {
            let bin = self
                .vibeke_bin
                .clone()
                .ok_or_else(|| unavailable("the vibeke binary is unknown"))?;
            let mut v = vec![
                bin.to_string_lossy().into_owned(),
                "sandbox".into(),
                "bwrap".into(),
                "--policy".into(),
                profile.to_string_lossy().into_owned(),
                "--cwd".into(),
                cwd.to_string_lossy().into_owned(),
                "--".into(),
            ];
            v.extend(argv.iter().cloned());
            v
        } else {
            crate::seatbelt::wrap_argv(&profile, argv)
        };
        Ok(Prepared {
            argv: wrapped,
            env,
            profile,
            policy,
        })
    }
}

/// The scrubbed host environment a restricted plugin inherits (the allowlist of
/// [`crate::env::scrub`]); the caller then adds its `HERDR_*` values.
pub fn scrubbed_env(host: impl IntoIterator<Item = (String, String)>) -> Vec<(String, String)> {
    let v: Vec<(String, String)> = host.into_iter().collect();
    env::scrub(&v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, PluginBox) {
        let t = tempfile::Builder::new()
            .prefix("vkpb")
            .tempdir_in("/tmp")
            .unwrap();
        let r = t.path().canonicalize().unwrap();
        for d in [
            "h",
            "s/plugins/state/a.b",
            "s/plugins/checkouts/a.b",
            "c/plugins/a.b",
            "s/prof",
        ] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        let b = PluginBox {
            home: r.join("h"),
            plugin_root: r.join("s/plugins/checkouts/a.b"),
            config_dir: r.join("c/plugins/a.b"),
            state_dir: r.join("s/plugins/state/a.b"),
            hidden: vec![r.join("s"), r.join("c")],
            extra_read: vec![],
            sockets: vec![],
            network: false,
            vibeke_bin: Some(PathBuf::from("/usr/bin/true")),
            profile_dir: r.join("s/prof"),
        };
        (t, b)
    }

    #[test]
    fn policy_shape() {
        let (_t, b) = fixture();
        let p = Policy::from_spec(&b.spec());
        assert!(p.allow_write.contains(&b.state_dir));
        assert!(
            !p.allow_write.contains(&b.plugin_root),
            "plugin dir is read-only"
        );
        assert!(p.allow_read_late.contains(&b.plugin_root));
        assert!(p.allow_read_late.contains(&b.config_dir));
        assert!(
            !p.allow_write.contains(&b.config_dir),
            "config is read-only"
        );
        assert!(
            p.network.is_none() && !p.network_open,
            "network off by default"
        );
        assert!(p.hidden.iter().any(|h| b.state_dir.starts_with(h)));
        let mut open = b.clone();
        open.network = true;
        assert!(Policy::from_spec(&open.spec()).network_open);
    }

    /// The Linux chain (generated on every platform): Vibeke's runtime dir is an empty tmpfs
    /// with only the invocation's own broker bound back, also with the network open (no
    /// `--unshare-net`); the broker registry is never re-exposed.
    #[test]
    fn linux_box_hides_the_runtime_dir_except_its_own_broker() {
        let (t, mut b) = fixture();
        let r = t.path().canonicalize().unwrap();
        let run = r.join("run/default/herdr-compat");
        std::fs::create_dir_all(run.join("brokers")).unwrap();
        std::fs::write(run.join("brokers.json"), "[]").unwrap();
        let own = run.join("brokers/own.sock");
        std::fs::write(&own, "").unwrap();
        b.hidden.push(r.join("run"));
        b.sockets = vec![own.clone()];
        b.network = true;
        let p = Policy::from_spec(&b.spec());
        assert!(!p.can_read(&run.join("brokers.json")));
        assert!(!p.can_read(&run.join("brokers")));
        let a = crate::linux::bwrap_args(&p, &b.plugin_root, None, None);
        let pos = |w: &[&str]| {
            a.windows(w.len())
                .position(|x| x.iter().zip(w).all(|(x, w)| x == w))
        };
        let run_s = r.join("run").to_string_lossy().into_owned();
        let own_s = own.to_string_lossy().into_owned();
        let hide = pos(&["--tmpfs", &run_s]).expect("runtime dir hidden");
        let bind = pos(&["--bind-try", &own_s, &own_s]).expect("own broker bound");
        assert!(bind > hide, "{a:?}");
        assert!(!a.iter().any(|x| x.contains("brokers.json")), "{a:?}");
        assert!(!a.iter().any(|x| x == "--unshare-net"), "network = true");
    }

    #[test]
    fn scrubbed_env_drops_secrets() {
        let host = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("GITHUB_TOKEN".to_string(), "x".to_string()),
        ];
        let e = scrubbed_env(host);
        assert_eq!(e, vec![("PATH".to_string(), "/usr/bin".to_string())]);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn profile_renders_for_seatbelt() {
        let (_t, b) = fixture();
        let sb = crate::seatbelt::render(&Policy::from_spec(&b.spec()));
        assert!(!sb.contains("(allow network-outbound)\n"), "{sb}");
        let mut open = b.clone();
        open.network = true;
        let sb = crate::seatbelt::render(&Policy::from_spec(&open.spec()));
        assert!(!sb.contains("(allow network-outbound)\n"), "{sb}");
        assert!(sb.contains("(remote ip \"*:*\")"), "{sb}");
    }
}
