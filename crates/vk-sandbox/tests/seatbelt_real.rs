//! Real `sandbox-exec` runs (macOS only; `sandbox-exec` ships with the OS). Each test builds a
//! fake home, a git repo with a task worktree, an inbox and a private dir under a temp root,
//! renders the profile and runs `/bin/sh -c …` inside it.
#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use vk_sandbox::creds::{self, HarnessAuth, ProjectionInput};
use vk_sandbox::net::{EgressPolicy, NetworkProfile};
use vk_sandbox::policy::{GitLayout, NetMode, Policy, SandboxSpec};
use vk_sandbox::proxy::{EgressProxy, ProxyConfig};
use vk_sandbox::seatbelt;

struct Fx {
    _t: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
    co: PathBuf,
    private: PathBuf,
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn fixture() -> Fx {
    // Short root: unix socket paths inside it must stay under ~104 bytes.
    let t = tempfile::Builder::new()
        .prefix("vks")
        .tempdir_in("/tmp")
        .unwrap();
    let root = t.path().canonicalize().unwrap();
    let home = root.join("h");
    std::fs::create_dir_all(home.join(".ssh")).unwrap();
    std::fs::write(home.join(".ssh/id_test"), "FAKE PRIVATE KEY\n").unwrap();
    std::fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = T\n\temail = t@example.invalid\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join("Desktop")).unwrap();
    std::fs::write(home.join("Desktop/shot.png"), "png").unwrap();
    let repo = home.join("code/repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README"), "hi\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "base"]);
    let co = home.join("code/repo-t");
    git(
        &repo,
        &["worktree", "add", "-q", "-b", "vk/t", co.to_str().unwrap()],
    );
    let private = root.join("s/sbx/p1");
    std::fs::create_dir_all(private.join("tmp")).unwrap();
    std::fs::create_dir_all(root.join("s/inbox/abc")).unwrap();
    std::fs::write(root.join("s/inbox/abc/drop.txt"), "dropped\n").unwrap();
    std::fs::write(root.join("s/state.db"), "db").unwrap();
    Fx {
        _t: t,
        root,
        home,
        repo,
        co,
        private,
    }
}

fn spec(fx: &Fx, network: NetMode) -> SandboxSpec {
    SandboxSpec {
        home: fx.home.clone(),
        checkout: fx.co.clone(),
        git: GitLayout::detect(&fx.co),
        private_dir: fx.private.clone(),
        extra_read: vec![fx.root.join("s/inbox")],
        extra_write: vec![],
        read_only_files: vec![],
        hidden: vec![fx.root.join("s")],
        home_read: None,
        unix_sockets: vec![],
        network,
        allow_bind_localhost: true,
        protected: vec![],
        control_dir: None,
    }
}

fn run(fx: &Fx, sp: &SandboxSpec, script: &str) -> Output {
    let policy = Policy::from_spec(sp);
    let profile = fx.private.join("profile.sb");
    std::fs::write(&profile, seatbelt::render(&policy)).unwrap();
    Command::new(seatbelt::SANDBOX_EXEC)
        .arg("-f")
        .arg(&profile)
        .args(["/bin/sh", "-c", script])
        .current_dir(&fx.co)
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .env("HOME", &fx.home)
        .env("TMPDIR", fx.private.join("tmp"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

fn lines(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn filesystem_allowlist() {
    let fx = fixture();
    let sp = spec(&fx, NetMode::None);
    let h = fx.home.display();
    let common = fx.repo.join(".git");
    let c = common.display();
    let script = format!(
        r#"
echo work > file.txt && echo W_CHECKOUT_OK
echo x > {p}/tmp/t && echo W_PRIVATE_OK
cat {h}/.gitconfig >/dev/null && echo R_GITCONFIG_OK
cat {inbox}/abc/drop.txt >/dev/null && echo R_INBOX_OK
git add file.txt && git commit -q -m sbx && echo GIT_COMMIT_OK
cat {h}/.ssh/id_test >/dev/null 2>&1 || echo R_SSH_DENIED
cat {h}/Desktop/shot.png >/dev/null 2>&1 || echo R_DESKTOP_DENIED
ls {h} >/dev/null 2>&1 || echo LS_HOME_DENIED
cat {state} >/dev/null 2>&1 || echo R_STATE_DENIED
echo x > {h}/planted 2>/dev/null || echo W_HOME_DENIED
echo x > {c}/hooks/pre-commit 2>/dev/null || echo W_HOOK_DENIED
echo x >> {c}/config 2>/dev/null || echo W_CONFIG_DENIED
echo 'gitdir: /tmp/evil' > .git 2>/dev/null || echo W_GITLINK_DENIED
mv {co} {co}.moved 2>/dev/null || echo MV_CHECKOUT_DENIED
cat {repo}/README >/dev/null 2>&1 || echo R_MAIN_CHECKOUT_DENIED
"#,
        p = fx.private.display(),
        inbox = fx.root.join("s/inbox").display(),
        state = fx.root.join("s/state.db").display(),
        co = fx.co.display(),
        repo = fx.repo.display(),
    );
    let o = run(&fx, &sp, &script);
    let out = lines(&o);
    for want in [
        "W_CHECKOUT_OK",
        "W_PRIVATE_OK",
        "R_GITCONFIG_OK",
        "R_INBOX_OK",
        "GIT_COMMIT_OK",
        "R_SSH_DENIED",
        "R_DESKTOP_DENIED",
        "LS_HOME_DENIED",
        "R_STATE_DENIED",
        "W_HOME_DENIED",
        "W_HOOK_DENIED",
        "W_CONFIG_DENIED",
        "W_GITLINK_DENIED",
        "MV_CHECKOUT_DENIED",
        "R_MAIN_CHECKOUT_DENIED",
    ] {
        assert!(out.contains(want), "missing {want}:\n{out}");
    }
    assert!(!fx.home.join("planted").exists());
    assert!(!common.join("hooks/pre-commit").exists());
    assert!(fx.co.exists());
    // The commit landed on the task branch, visible from the host.
    let log = Command::new("git")
        .arg("-C")
        .arg(&fx.repo)
        .args(["log", "-1", "--format=%s", "vk/t"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "sbx");
}

#[test]
fn real_home_is_not_readable_or_writable() {
    let Some(real_home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    let fx = fixture();
    let mut sp = spec(&fx, NetMode::None);
    sp.home = real_home.clone();
    let probe = real_home.join(format!(".vk-sbx-probe-{}", std::process::id()));
    let script = format!(
        "ls {h} >/dev/null 2>&1 || echo LS_DENIED\n: > {probe} 2>/dev/null || echo W_DENIED\n",
        h = real_home.display(),
        probe = probe.display()
    );
    let out = lines(&run(&fx, &sp, &script));
    let leaked = probe.exists();
    let _ = std::fs::remove_file(&probe);
    assert!(!leaked, "wrote into the real home");
    assert!(
        out.contains("LS_DENIED") && out.contains("W_DENIED"),
        "{out}"
    );
}

/// Tiny HTTP origin on 127.0.0.1.
fn origin() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        use std::io::{Read, Write};
        for mut s in l.incoming().flatten() {
            let mut buf = [0u8; 2048];
            let _ = s.read(&mut buf);
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nok\n",
            );
        }
    });
    port
}

#[test]
fn network_none_blocks_everything() {
    let fx = fixture();
    let o = origin();
    let sp = spec(&fx, NetMode::None);
    let out = lines(&run(
        &fx,
        &sp,
        &format!(
            "curl -sS -m 3 http://127.0.0.1:{o}/ && echo DIRECT_OK || echo DIRECT_DENIED\ncurl -sS -m 3 https://example.com -o /dev/null && echo NET_OK || echo NET_DENIED"
        ),
    ));
    assert!(
        out.contains("DIRECT_DENIED") && !out.contains("DIRECT_OK"),
        "{out}"
    );
    assert!(out.contains("NET_DENIED"), "{out}");
}

#[test]
fn proxy_only_reaches_the_proxy() {
    let fx = fixture();
    let o = origin();
    let other = origin();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut pol = EgressPolicy::new(NetworkProfile::HarnessApis);
    pol.local_ports.insert(o); // reachable *through* the proxy only
    let proxy = rt
        .block_on(EgressProxy::start(ProxyConfig::new(pol), 0, None))
        .unwrap();
    let sp = spec(
        &fx,
        NetMode::Proxy {
            port: proxy.port,
            local_ports: vec![],
        },
    );
    let pp = proxy.port;
    let script = format!(
        r#"
curl -sS -m 5 -x http://127.0.0.1:{pp} http://127.0.0.1:{o}/ && echo VIA_PROXY_OK
curl -sS -m 3 http://127.0.0.1:{o}/ >/dev/null 2>&1 && echo DIRECT_OK || echo DIRECT_DENIED
curl -s -m 5 -o /dev/null -w '%{{http_code}}\n' -x http://127.0.0.1:{pp} http://127.0.0.1:{other}/ | sed 's/^/OTHER_/'
curl -sS -m 3 --noproxy '*' https://example.com -o /dev/null 2>/dev/null && echo INTERNET_OK || echo INTERNET_DENIED
"#
    );
    let out = lines(&run(&fx, &sp, &script));
    assert!(out.contains("ok\nVIA_PROXY_OK"), "{out}");
    assert!(
        out.contains("DIRECT_DENIED") && !out.contains("DIRECT_OK"),
        "{out}"
    );
    assert!(out.contains("OTHER_403"), "{out}");
    assert!(out.contains("INTERNET_DENIED"), "{out}");
    drop(proxy);
}

#[test]
fn unix_sockets_only_the_broker() {
    let fx = fixture();
    let broker = fx.private.join("b.sock");
    let main = fx.root.join("s/vibeke.sock");
    let lb = std::os::unix::net::UnixListener::bind(&broker).unwrap();
    let _lm = std::os::unix::net::UnixListener::bind(&main).unwrap();
    std::thread::spawn(move || {
        use std::io::Write;
        for mut s in lb.incoming().flatten() {
            let _ = s.write_all(b"BROKER_HELLO\n");
        }
    });
    let mut sp = spec(&fx, NetMode::None);
    sp.unix_sockets = vec![broker.clone()];
    let script = format!(
        "nc -U {b} </dev/null | head -1\nnc -U {m} </dev/null >/dev/null 2>&1 && echo MAIN_OK || echo MAIN_DENIED\n",
        b = broker.display(),
        m = main.display()
    );
    let out = lines(&run(&fx, &sp, &script));
    assert!(out.contains("BROKER_HELLO"), "{out}");
    assert!(out.contains("MAIN_DENIED"), "{out}");
}

#[test]
fn projected_credentials_are_read_only() {
    let fx = fixture();
    std::fs::create_dir_all(fx.home.join(".codex")).unwrap();
    std::fs::write(
        fx.home.join(".codex/auth.json"),
        "{\"token\":\"FAKE-codex-token-123456\"}",
    )
    .unwrap();
    let creds_dir = fx.root.join("s/credentials");
    std::fs::create_dir_all(&creds_dir).unwrap();
    std::fs::set_permissions(&creds_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let pr = creds::project(
        HarnessAuth::Codex,
        &ProjectionInput {
            home: &fx.home,
            host_env: &[],
            vibeke_credentials: &creds_dir,
            private_dir: &fx.private,
            trust_checkout: None,
            claude_dir: None,
            codex_dir: None,
            pi_agent_dir: None,
        },
    )
    .unwrap();
    let mut sp = spec(&fx, NetMode::None);
    sp.read_only_files = pr.read_only_files.clone();
    sp.extra_write = pr.write.clone();
    let codex_home = pr
        .env
        .iter()
        .find(|(k, _)| k == "CODEX_HOME")
        .map(|(_, v)| v.clone())
        .unwrap();
    let script = format!(
        r#"
cat {ch}/auth.json
echo
echo x >> {ch}/auth.json 2>/dev/null || echo W_CRED_DENIED
rm -f {ch}/auth.json 2>/dev/null; test -f {ch}/auth.json && echo RM_CRED_DENIED
echo s > {ch}/session.jsonl && echo W_EPHEMERAL_OK
cat {h}/.codex/auth.json >/dev/null 2>&1 || echo R_HOST_CRED_DENIED
"#,
        ch = codex_home,
        h = fx.home.display()
    );
    let out = lines(&run(&fx, &sp, &script));
    assert!(out.contains("FAKE-codex-token-123456"), "{out}");
    for want in [
        "W_CRED_DENIED",
        "RM_CRED_DENIED",
        "W_EPHEMERAL_OK",
        "R_HOST_CRED_DENIED",
    ] {
        assert!(out.contains(want), "missing {want}:\n{out}");
    }
    // The host credential file is untouched.
    assert_eq!(
        std::fs::read_to_string(fx.home.join(".codex/auth.json")).unwrap(),
        "{\"token\":\"FAKE-codex-token-123456\"}"
    );
}

#[test]
fn git_executed_files_inside_the_checkout_are_write_protected() {
    let fx = fixture();
    // The repo runs checkout-local hooks, an fsmonitor script and an included config file.
    git(&fx.co, &["config", "core.hooksPath", ".githooks"]);
    git(&fx.co, &["config", "core.fsmonitor", "./tools/fsmon.sh"]);
    git(
        &fx.co,
        &["config", "include.path", "../../repo-t/.gitconfig.local"],
    );
    std::fs::create_dir_all(fx.co.join(".githooks")).unwrap();
    std::fs::write(fx.co.join(".githooks/pre-commit"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::create_dir_all(fx.co.join("tools")).unwrap();
    std::fs::write(fx.co.join("tools/fsmon.sh"), "#!/bin/sh\n").unwrap();
    let mut sp = spec(&fx, NetMode::None);
    sp.protected = vk_sandbox::gitexec::exec_targets(&fx.co, Some(&fx.home));
    for want in [".githooks", "tools/fsmon.sh", ".gitconfig.local"] {
        assert!(
            sp.protected.contains(&fx.co.join(want)),
            "{want} not protected: {:?}",
            sp.protected
        );
    }
    let out = lines(&run(
        &fx,
        &sp,
        r#"
echo 'touch /tmp/pwned' >> .githooks/pre-commit 2>/dev/null || echo HOOK_DENIED
echo x > .githooks/post-checkout 2>/dev/null || echo NEW_HOOK_DENIED
echo 'touch /tmp/pwned' > tools/fsmon.sh 2>/dev/null || echo FSMON_DENIED
printf '[core]\n\tfsmonitor = /tmp/x\n' > .gitconfig.local 2>/dev/null || echo INCLUDE_DENIED
echo ok > src.txt && echo NORMAL_WRITE_OK
"#,
    ));
    for want in [
        "HOOK_DENIED",
        "NEW_HOOK_DENIED",
        "FSMON_DENIED",
        "INCLUDE_DENIED",
        "NORMAL_WRITE_OK",
    ] {
        assert!(out.contains(want), "missing {want}:\n{out}");
    }
    assert!(!fx.co.join(".gitconfig.local").exists());
}

/// Helper re-executed inside the sandbox by [`tiocsti_cannot_inject_into_parent`]: pushes
/// `$VK_STI_TEXT` into the terminal's input queue with `TIOCSTI`. A no-op in normal test runs.
#[test]
fn tiocsti_helper() {
    let Ok(text) = std::env::var("VK_STI_TEXT") else {
        return;
    };
    let mut denied = false;
    for b in format!("{text}\n").bytes() {
        // SAFETY: ioctl on stdin with a pointer to one valid byte.
        let r = unsafe { libc::ioctl(0, seatbelt::TIOCSTI as libc::c_ulong, &b as *const u8) };
        if r != 0 {
            denied = true;
        }
    }
    println!("{}", if denied { "STI_DENIED" } else { "STI_DONE" });
}

/// A host shell on its own pty runs `cmd`, then `exit`; returns everything it printed.
fn host_shell_runs(cmd: &str, env: &[(&str, String)]) -> String {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;
    let (mut master, mut slave) = (0, 0);
    // SAFETY: openpty with valid out-pointers.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(r, 0);
    let mut c = Command::new("/bin/sh");
    c.arg("-i")
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("PS1", "$ ");
    for (k, v) in env {
        c.env(k, v);
    }
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        c.pre_exec(move || {
            libc::setsid();
            libc::ioctl(slave, libc::TIOCSCTTY as libc::c_ulong, 0);
            libc::dup2(slave, 0);
            libc::dup2(slave, 1);
            libc::dup2(slave, 2);
            Ok(())
        });
    }
    let mut child = c.spawn().unwrap();
    // SAFETY: the parent no longer needs the slave end.
    unsafe { libc::close(slave) };
    // SAFETY: we own `master`.
    let mut m = unsafe { <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(master) };
    let mut rd = m.try_clone().unwrap();
    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let o2 = out.clone();
    std::thread::spawn(move || {
        let mut b = [0u8; 4096];
        while let Ok(n) = rd.read(&mut b) {
            if n == 0 {
                break;
            }
            o2.lock().unwrap().extend_from_slice(&b[..n]);
        }
    });
    std::thread::sleep(std::time::Duration::from_millis(300));
    m.write_all(format!("{cmd}\n").as_bytes()).unwrap();
    // Wait for the helper to finish (and for any injected line to run).
    for _ in 0..100 {
        let s = String::from_utf8_lossy(&out.lock().unwrap()).into_owned();
        if s.contains("STI_DENIED") || s.contains("STI_DONE") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    std::thread::sleep(std::time::Duration::from_millis(700));
    let _ = m.write_all(b"exit\n");
    for _ in 0..50 {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = child.kill();
    String::from_utf8_lossy(&out.lock().unwrap()).into_owned()
}

/// 13 §15 / review finding 5: an isolated command typed into an unsandboxed host shell shares
/// that shell's terminal. `TIOCSTI` from inside the sandbox must not queue a command for the
/// parent shell (host-side execution sentinel).
#[test]
fn tiocsti_cannot_inject_into_parent() {
    let fx = fixture();
    // The helper is this test binary, copied where the profile lets the box execute it.
    let exe = fx.co.join("tiocsti-helper");
    std::fs::copy(std::env::current_exe().unwrap(), &exe).unwrap();
    let sentinel = fx.root.join("sti-sentinel");
    let helper = format!(
        "{} --exact tiocsti_helper --nocapture --test-threads=1",
        exe.display()
    );
    let env = [("VK_STI_TEXT", format!("touch {}", sentinel.display()))];
    // Control: unsandboxed, the injection works on this OS (otherwise there is nothing to test).
    let out = host_shell_runs(&helper, &env);
    let control = sentinel.exists();
    let _ = std::fs::remove_file(&sentinel);
    if !control {
        eprintln!("TIOCSTI injection does not work here even unsandboxed; skipping\n{out}");
        return;
    }
    // Sandboxed with Vibeke's profile: denied, nothing runs in the parent shell.
    let sp = spec(&fx, NetMode::None);
    let profile = fx.private.join("sti.sb");
    std::fs::write(&profile, seatbelt::render(&Policy::from_spec(&sp))).unwrap();
    let out = host_shell_runs(
        &format!(
            "cd {} && {} -f {} {helper}",
            fx.co.display(),
            seatbelt::SANDBOX_EXEC,
            profile.display()
        ),
        &env,
    );
    assert!(out.contains("STI_DENIED"), "{out}");
    assert!(
        !sentinel.exists(),
        "a sandboxed process injected a command into its parent shell:\n{out}"
    );
}

#[test]
fn dev_server_can_listen_on_localhost() {
    let fx = fixture();
    let sp = spec(&fx, NetMode::None);
    // nc -l binds 127.0.0.1; the bind itself must be allowed (previews, 13 §7 inbound).
    let out = lines(&run(
        &fx,
        &sp,
        "nc -l 127.0.0.1 38917 & P=$!; sleep 0.3; kill $P 2>/dev/null && echo LISTEN_OK",
    ));
    assert!(out.contains("LISTEN_OK"), "{out}");
}
