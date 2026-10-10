//! `vibeke cloud exec` (spec 17 §3.2): a `docker exec`-shaped command over
//! [`Provider::exec`](crate::Provider::exec).
//!
//! ```text
//! vibeke cloud exec [-i] [-t] [-w DIR] [-e K=V]... [--session-file PATH] <provider>/<box-id> [--] CMD [ARG]...
//! ```
//!
//! - `-t`: terminal mode. Local stdin goes raw when it is a TTY, the size follows the local
//!   terminal (`SIGWINCH`), and the session is detachable. With `--session-file`, an active
//!   session named in the file is attached instead of starting `CMD`; a new session's id is
//!   written to the file (0600). A lost connection attaches again with backoff. At startup,
//!   transient errors while checking that session are retried for up to a minute; a new
//!   `CMD` starts only when the provider reports the session gone (else exit 125).
//! - `-i` without `-t`: pipe mode. stdin is sent, then EOF; stdout and stderr stay separate.
//! - Exit codes: the remote command's code; 125 when it can't run (no credential: stderr
//!   `vibeke: needs_auth <provider>`); 255 when the connection is lost in pipe mode.
//!
//! The credential is resolved here with [`auth::require`]; it never travels in argv or env.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::mpsc;

use crate::{CloudConfig, ErrorKind, ExecReq, In, Out, Provider, Secret, Session, auth};

pub const USAGE: &str = "usage: vibeke cloud exec [-i] [-t] [-w DIR] [-e K=V]... [--session-file PATH] <provider>/<box-id> [--] CMD [ARG]...";

const EXIT_CANT_RUN: i32 = 125;
const EXIT_LOST: i32 = 255;

#[derive(Debug, Default, PartialEq, Eq)]
struct Opts {
    interactive: bool,
    tty: bool,
    workdir: Option<String>,
    env: Vec<(String, String)>,
    session_file: Option<PathBuf>,
    provider: String,
    box_id: String,
    cmd: Vec<String>,
}

fn parse(args: &[String], getenv: &dyn Fn(&str) -> Option<String>) -> Result<Opts, String> {
    let mut o = Opts::default();
    let mut i = 0;
    let value = |i: &mut usize, flag: &str| -> Result<String, String> {
        *i += 1;
        args.get(*i)
            .cloned()
            .ok_or_else(|| format!("{flag} needs a value"))
    };
    let push_env = |o: &mut Opts, kv: String| -> Result<(), String> {
        match kv.split_once('=') {
            Some((k, v)) if !k.is_empty() => o.env.push((k.to_string(), v.to_string())),
            None if !kv.is_empty() => {
                // `-e NAME` passes the local value, like docker.
                if let Some(v) = getenv(&kv) {
                    o.env.push((kv, v));
                }
            }
            _ => return Err(format!("bad -e value {kv:?}")),
        }
        Ok(())
    };
    while i < args.len() {
        let a = args[i].as_str();
        match a {
            "-i" | "--interactive" => o.interactive = true,
            "-t" | "--tty" => o.tty = true,
            "-w" | "--workdir" => o.workdir = Some(value(&mut i, a)?),
            "-e" | "--env" => {
                let kv = value(&mut i, a)?;
                push_env(&mut o, kv)?;
            }
            "--session-file" => o.session_file = Some(PathBuf::from(value(&mut i, a)?)),
            "-h" | "--help" => return Err(String::new()),
            _ if a.starts_with("--workdir=") => o.workdir = Some(a["--workdir=".len()..].into()),
            _ if a.starts_with("--env=") => push_env(&mut o, a["--env=".len()..].into())?,
            _ if a.starts_with("--session-file=") => {
                o.session_file = Some(PathBuf::from(&a["--session-file=".len()..]))
            }
            // Combined short flags: -it, -ti, -itw DIR is not supported (docker doesn't either).
            _ if a.len() > 2
                && a.starts_with('-')
                && !a.starts_with("--")
                && a[1..].bytes().all(|b| b == b'i' || b == b't') =>
            {
                o.interactive |= a.contains('i');
                o.tty |= a.contains('t');
            }
            _ if a.starts_with('-') && a != "-" => return Err(format!("unknown flag {a}")),
            _ => break,
        }
        i += 1;
    }
    let target = args.get(i).ok_or("missing <provider>/<box-id>")?;
    let (p, b) = target
        .split_once('/')
        .filter(|(p, b)| !p.is_empty() && !b.is_empty())
        .ok_or_else(|| format!("expected <provider>/<box-id>, got {target:?}"))?;
    o.provider = p.to_string();
    o.box_id = b.to_string();
    i += 1;
    if args.get(i).map(String::as_str) == Some("--") {
        i += 1;
    }
    o.cmd = args[i.min(args.len())..].to_vec();
    if o.cmd.is_empty() {
        return Err("missing CMD".into());
    }
    Ok(o)
}

/// Run `vibeke cloud exec <args>`; returns the process exit code. Dispatch it before any
/// runtime or thread exists: `--fake-daemon` forks.
pub fn run(args: &[String]) -> i32 {
    if args.first().map(String::as_str) == Some("--fake-daemon") {
        return crate::fake::session_daemon_main(&args[1..]);
    }
    let o = match parse(args, &|k| std::env::var(k).ok()) {
        Ok(o) => o,
        Err(e) if e.is_empty() => {
            println!("{USAGE}");
            return 0;
        }
        Err(e) => {
            eprintln!("vibeke cloud exec: {e}\n{USAGE}");
            return EXIT_CANT_RUN;
        }
    };
    let rt = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("vibeke cloud exec: {e}");
            return EXIT_CANT_RUN;
        }
    };
    let code = rt.block_on(main_async(o));
    // The stdin thread may still be blocked in read(2); don't wait for it.
    rt.shutdown_background();
    code
}

async fn main_async(o: Opts) -> i32 {
    let cfg = CloudConfig::load();
    let Some(p) = crate::provider(&cfg, &o.provider) else {
        eprintln!("vibeke: unknown cloud provider {:?}", o.provider);
        return EXIT_CANT_RUN;
    };
    let vk_cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let kc = auth::keychain(&vk_cfg).unwrap_or(vk_store::keychain::Keychain::Os);
    let cred = match auth::require(&*p, &cfg, &kc, &|k| std::env::var(k).ok()) {
        Ok(c) => c,
        Err(e) => return fail(&o.provider, &e),
    };
    if o.tty {
        run_tty(p, cred, o).await
    } else {
        run_pipe(p, cred, o).await
    }
}

/// Report an error that kept the command from running.
fn fail(provider: &str, e: &crate::CloudError) -> i32 {
    if e.kind == ErrorKind::NeedsAuth {
        eprintln!("vibeke: needs_auth {provider}");
    } else {
        eprintln!("vibeke: {e}");
    }
    EXIT_CANT_RUN
}

/// Read local stdin on a plain thread (a blocked read must not hold up exit).
fn stdin_reader() -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel(16);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; 32 * 1024];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    });
    rx
}

async fn run_pipe(p: Arc<dyn Provider>, cred: Secret, o: Opts) -> i32 {
    let req = ExecReq {
        argv: o.cmd.clone(),
        env: o.env.clone(),
        cwd: o.workdir.clone(),
        tty: false,
        cols: 0,
        rows: 0,
        detachable: false,
    };
    let mut s = match p.exec(&cred, &o.box_id, req).await {
        Ok(s) => s,
        Err(e) => return fail(&o.provider, &e),
    };
    // stdin is fed from its own task: a full input channel (the box not reading yet) must not
    // stop this loop from draining output, or large traffic both ways deadlocks.
    if o.interactive {
        let input = s.input.clone();
        let mut rx = stdin_reader();
        tokio::spawn(async move {
            while let Some(d) = rx.recv().await {
                if input.send(In::Data(d)).await.is_err() {
                    return;
                }
            }
            let _ = input.send(In::Eof).await;
        });
    } else {
        let _ = s.input.send(In::Eof).await;
    }
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    loop {
        match s.output.recv().await {
            Some(Out::Stdout(d)) => {
                if stdout.write_all(&d).await.is_err() || stdout.flush().await.is_err() {
                    return EXIT_LOST;
                }
            }
            Some(Out::Stderr(d)) => {
                let _ = stderr.write_all(&d).await;
                let _ = stderr.flush().await;
            }
            Some(Out::Exit(c)) => {
                let _ = stdout.flush().await;
                return c;
            }
            Some(Out::PortOpened { .. }) => {}
            Some(Out::Lost(m)) => {
                eprintln!(
                    "vibeke: connection to {}/{} lost: {m}",
                    o.provider, o.box_id
                );
                return EXIT_LOST;
            }
            None => {
                eprintln!("vibeke: connection to {}/{} lost", o.provider, o.box_id);
                return EXIT_LOST;
            }
        }
    }
}

/// Next chunk from an optional receiver; pending forever once it is gone.
async fn recv_opt(rx: &mut Option<mpsc::Receiver<Vec<u8>>>) -> Option<Vec<u8>> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

// ---- terminal mode ----

/// Raw mode on local stdin, restored on drop (normal return, `?`, or panic unwind).
struct RawMode(Option<libc::termios>);

impl RawMode {
    fn enter() -> RawMode {
        // SAFETY: isatty/tcgetattr/tcsetattr on fd 0 with a termios we own.
        unsafe {
            if libc::isatty(0) != 1 {
                return RawMode(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return RawMode(None);
            }
            let saved = t;
            libc::cfmakeraw(&mut t);
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return RawMode(None);
            }
            RawMode(Some(saved))
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        if let Some(t) = self.0 {
            // SAFETY: restoring the attributes read in `enter`.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, &t);
            }
        }
    }
}

/// Local terminal size, 80x24 when unknown.
fn term_size() -> (u16, u16) {
    for fd in [0, 1, 2] {
        // SAFETY: TIOCGWINSZ fills a winsize we own.
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let r = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
        if r == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
}

fn read_session_file(p: &Path) -> Option<String> {
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim();
    (!s.is_empty()).then(|| s.to_string())
}

fn write_session_file(p: &Path, id: &str) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(dir) = p.parent().filter(|d| !d.as_os_str().is_empty()) {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = p.with_extension("tmp");
    let r = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .and_then(|mut f| f.write_all(id.as_bytes()))
        .and_then(|_| std::fs::rename(&tmp, p));
    if let Err(e) = r {
        eprintln!("vibeke: can't write the session file: {e}");
    }
}

/// How long startup keeps retrying a session named in `--session-file` before it gives up
/// (without starting a second copy of the command).
const RESUME_BUDGET: Duration = Duration::from_secs(60);

/// Errors that may clear up on their own; the caller retries them with backoff.
fn transient(k: ErrorKind) -> bool {
    matches!(
        k,
        ErrorKind::Unavailable | ErrorKind::RateLimited | ErrorKind::Internal
    )
}

/// What to do with the session named in `--session-file`.
#[derive(Debug, PartialEq, Eq)]
enum Resume {
    /// The provider lists it as active: attach.
    Attach,
    /// The provider positively reports it gone: start a new command.
    Gone,
    /// The answer was a transient failure (its message): ask again.
    Retry(String),
}

/// Decide from the provider's session list. Non-transient errors other than `NotFound`
/// (the box itself is gone) are returned: they must not start a replacement command.
fn classify_sessions(
    r: Result<Vec<crate::SessionInfo>, crate::CloudError>,
    id: &str,
) -> Result<Resume, crate::CloudError> {
    match r {
        Ok(list) if list.iter().any(|s| s.id == id && s.active) => Ok(Resume::Attach),
        Ok(_) => Ok(Resume::Gone),
        Err(e) if e.kind == ErrorKind::NotFound => Ok(Resume::Gone),
        Err(e) if transient(e.kind) => Ok(Resume::Retry(e.message)),
        Err(e) => Err(e),
    }
}

/// Attach to the session named in `--session-file` when the provider reports it active.
/// Returns `Ok(None)` only when the provider positively reports that session gone, so a
/// replacement command may start. Transient failures are retried with the reconnect backoff
/// for up to [`RESUME_BUDGET`]; after that the error is returned and nothing new starts.
async fn try_resume(
    p: &dyn Provider,
    cred: &Secret,
    o: &Opts,
    cols: u16,
    rows: u16,
) -> Result<Option<Session>, crate::CloudError> {
    let Some(file) = o.session_file.as_deref() else {
        return Ok(None);
    };
    let Some(id) = read_session_file(file) else {
        return Ok(None);
    };
    let deadline = tokio::time::Instant::now() + RESUME_BUDGET;
    let mut delay = Duration::from_millis(500);
    let mut announced = false;
    loop {
        let last = match classify_sessions(p.sessions(cred, &o.box_id).await, &id)? {
            Resume::Gone => return Ok(None),
            Resume::Attach => match p.attach(cred, &o.box_id, &id, cols, rows).await {
                Ok(s) => return Ok(Some(s)),
                // Ended between the listing and the attach.
                Err(e) if e.kind == ErrorKind::NotFound => return Ok(None),
                Err(e) if transient(e.kind) => e.message,
                Err(e) => return Err(e),
            },
            Resume::Retry(m) => m,
        };
        tracing::debug!(reason = %last, "cloud exec resume failed; retrying");
        let now = tokio::time::Instant::now();
        if now + delay > deadline {
            return Err(crate::CloudError::new(
                ErrorKind::Unavailable,
                format!(
                    "can't reach session {id} in {}/{} ({last}); not starting a new command \
                     while it may still run. Try again later, or delete {} to start fresh",
                    o.provider,
                    o.box_id,
                    file.display()
                ),
            ));
        }
        if !announced {
            announced = true;
            let mut stdout = tokio::io::stdout();
            let _ = stdout
                .write_all(
                    format!(
                        "\r\n\x1b[2m[vibeke: reconnecting to {}…]\x1b[0m\r\n",
                        o.box_id
                    )
                    .as_bytes(),
                )
                .await;
            let _ = stdout.flush().await;
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_secs(10));
    }
}

/// Attach to the session in `--session-file`, or start `CMD` (recording its id there).
async fn start_tty(
    p: &dyn Provider,
    cred: &Secret,
    o: &Opts,
    cols: u16,
    rows: u16,
) -> Result<Session, crate::CloudError> {
    if let Some(s) = try_resume(p, cred, o, cols, rows).await? {
        return Ok(s);
    }
    let req = ExecReq {
        argv: o.cmd.clone(),
        env: o.env.clone(),
        cwd: o.workdir.clone(),
        tty: true,
        cols,
        rows,
        detachable: true,
    };
    let s = p.exec(cred, &o.box_id, req).await?;
    if let Some(f) = &o.session_file
        && !s.id.is_empty()
    {
        write_session_file(f, &s.id);
    }
    Ok(s)
}

async fn run_tty(p: Arc<dyn Provider>, cred: Secret, o: Opts) -> i32 {
    let (cols, rows) = term_size();
    // Signals are handled from the start: a pane closed while the box is still being reached
    // (startup resume, exec, reattach) must end this process, not wait for the provider.
    let (Ok(mut winch), Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::window_change()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        eprintln!("vibeke: can't install signal handlers");
        return EXIT_CANT_RUN;
    };
    let started = tokio::select! {
        r = start_tty(&*p, &cred, &o, cols, rows) => r,
        _ = term.recv() => return 143,
        _ = hup.recv() => return 129,
    };
    let mut session = match started {
        Ok(s) => s,
        Err(e) => return fail(&o.provider, &e),
    };
    let sid = session.id.clone();
    let _raw = RawMode::enter();
    let mut stdin = Some(stdin_reader());
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    loop {
        // Bridge until the session ends or the connection drops.
        let lost = loop {
            tokio::select! {
                d = recv_opt(&mut stdin) => match d {
                    Some(d) => {
                        if session.input.send(In::Data(d)).await.is_err() {
                            break "session closed".to_string();
                        }
                    }
                    None => stdin = None,
                },
                _ = winch.recv() => {
                    let (cols, rows) = term_size();
                    let _ = session.input.send(In::Resize { cols, rows }).await;
                }
                _ = term.recv() => return 143,
                _ = hup.recv() => return 129,
                out = session.output.recv() => match out {
                    Some(Out::Stdout(d)) => {
                        let _ = stdout.write_all(&d).await;
                        let _ = stdout.flush().await;
                    }
                    Some(Out::Stderr(d)) => {
                        let _ = stderr.write_all(&d).await;
                        let _ = stderr.flush().await;
                    }
                    Some(Out::Exit(c)) => return c,
                    Some(Out::PortOpened { .. }) => {}
                    Some(Out::Lost(m)) => break m,
                    None => break "connection closed".to_string(),
                },
            }
        };
        tracing::debug!(reason = %lost, "cloud exec connection lost");
        if sid.is_empty() {
            eprint!(
                "\r\nvibeke: connection to {}/{} lost\r\n",
                o.provider, o.box_id
            );
            return EXIT_LOST;
        }
        // Attach again with backoff, without limit; one status line per outage.
        let _ = stdout
            .write_all(
                format!(
                    "\r\n\x1b[2m[vibeke: reconnecting to {}…]\x1b[0m\r\n",
                    o.box_id
                )
                .as_bytes(),
            )
            .await;
        let _ = stdout.flush().await;
        let mut delay = Duration::from_millis(500);
        session = loop {
            tokio::select! {
                _ = tokio::time::sleep(delay) => {}
                _ = term.recv() => return 143,
                _ = hup.recv() => return 129,
            }
            let (cols, rows) = term_size();
            let attached = tokio::select! {
                r = p.attach(&cred, &o.box_id, &sid, cols, rows) => r,
                _ = term.recv() => return 143,
                _ = hup.recv() => return 129,
            };
            match attached {
                Ok(s) => break s,
                Err(e) if e.kind == ErrorKind::NeedsAuth => {
                    drop(_raw);
                    return fail(&o.provider, &e);
                }
                Err(e) if e.kind == ErrorKind::NotFound => {
                    eprint!("\r\nvibeke: the session in {} has ended\r\n", o.box_id);
                    return EXIT_LOST;
                }
                Err(_) => delay = (delay * 2).min(Duration::from_secs(10)),
            }
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn parses_docker_style_flags() {
        let env = |k: &str| (k == "LANG").then(|| "C".to_string());
        let o = parse(
            &a(&[
                "-it",
                "-w",
                "/workspace",
                "-e",
                "TERM=xterm",
                "-e",
                "LANG",
                "-e",
                "MISSING",
                "--session-file",
                "/tmp/s",
                "sprites/vk-1",
                "--",
                "bash",
                "-l",
            ]),
            &env,
        )
        .unwrap();
        assert!(o.interactive && o.tty);
        assert_eq!(o.workdir.as_deref(), Some("/workspace"));
        assert_eq!(
            o.env,
            vec![
                ("TERM".to_string(), "xterm".to_string()),
                ("LANG".to_string(), "C".to_string())
            ]
        );
        assert_eq!(o.session_file, Some(PathBuf::from("/tmp/s")));
        assert_eq!(
            (o.provider.as_str(), o.box_id.as_str()),
            ("sprites", "vk-1")
        );
        assert_eq!(o.cmd, a(&["bash", "-l"]));

        let o = parse(&a(&["-i", "fake/b", "git", "upload-pack", "-x"]), &env).unwrap();
        assert!(o.interactive && !o.tty);
        assert_eq!(o.cmd, a(&["git", "upload-pack", "-x"]));

        assert!(parse(&a(&["-i", "fake/b"]), &env).is_err());
        assert!(parse(&a(&["-i", "nobox", "ls"]), &env).is_err());
        assert!(parse(&a(&["-x", "fake/b", "ls"]), &env).is_err());
        assert_eq!(parse(&a(&["--help"]), &env).unwrap_err(), "");
    }

    fn info(id: &str, active: bool) -> crate::SessionInfo {
        crate::SessionInfo {
            id: id.into(),
            command: "bash".into(),
            tty: true,
            active,
            last_activity_at: 0,
        }
    }

    #[test]
    fn resume_starts_a_replacement_only_when_the_session_is_gone() {
        use crate::CloudError;
        let c = |r| classify_sessions(r, "s1");
        assert_eq!(c(Ok(vec![info("s1", true)])), Ok(Resume::Attach));
        assert_eq!(c(Ok(vec![info("s1", false)])), Ok(Resume::Gone));
        assert_eq!(c(Ok(vec![info("s2", true)])), Ok(Resume::Gone));
        assert_eq!(c(Ok(vec![])), Ok(Resume::Gone));
        assert_eq!(c(Err(CloudError::not_found("gone"))), Ok(Resume::Gone));
        for k in [
            ErrorKind::Unavailable,
            ErrorKind::RateLimited,
            ErrorKind::Internal,
        ] {
            assert_eq!(
                c(Err(CloudError::new(k, "x"))),
                Ok(Resume::Retry("x".into())),
                "{k:?}"
            );
        }
        for k in [
            ErrorKind::NeedsAuth,
            ErrorKind::Account,
            ErrorKind::Unsupported,
        ] {
            assert!(c(Err(CloudError::new(k, "x"))).is_err(), "{k:?}");
        }
    }

    #[test]
    fn a_quiet_sprites_session_is_attached_not_replaced() {
        // Sprites reports `is_active: false` for a session without recent output. It still runs.
        let listed = crate::sprites::sessions_from(&serde_json::json!({"sessions": [
            {"id": "s1", "command": "bash", "tty": true, "is_active": false,
             "last_activity": "2026-01-02T04:00:00Z"}
        ]}));
        assert_eq!(classify_sessions(Ok(listed), "s1"), Ok(Resume::Attach));
    }
}
