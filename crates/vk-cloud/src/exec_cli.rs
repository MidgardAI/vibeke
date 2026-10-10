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
//!   written to the file (0600). A lost connection attaches again with backoff.
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
    let mut stdin = if o.interactive {
        Some(stdin_reader())
    } else {
        let _ = s.input.send(In::Eof).await;
        None
    };
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    loop {
        tokio::select! {
            d = recv_opt(&mut stdin) => match d {
                Some(d) => {
                    let _ = s.input.send(In::Data(d)).await;
                }
                None => {
                    stdin = None;
                    let _ = s.input.send(In::Eof).await;
                }
            },
            out = s.output.recv() => match out {
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
                    eprintln!("vibeke: connection to {}/{} lost: {m}", o.provider, o.box_id);
                    return EXIT_LOST;
                }
                None => {
                    eprintln!("vibeke: connection to {}/{} lost", o.provider, o.box_id);
                    return EXIT_LOST;
                }
            },
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

/// Attach to the session named in `--session-file` when the provider reports it active.
async fn try_resume(
    p: &dyn Provider,
    cred: &Secret,
    o: &Opts,
    cols: u16,
    rows: u16,
) -> Result<Option<Session>, crate::CloudError> {
    let Some(id) = o.session_file.as_deref().and_then(read_session_file) else {
        return Ok(None);
    };
    let sessions = match p.sessions(cred, &o.box_id).await {
        Ok(s) => s,
        Err(e) if e.kind == ErrorKind::NeedsAuth => return Err(e),
        Err(_) => return Ok(None),
    };
    if !sessions.iter().any(|s| s.id == id && s.active) {
        return Ok(None);
    }
    match p.attach(cred, &o.box_id, &id, cols, rows).await {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind == ErrorKind::NeedsAuth => Err(e),
        Err(_) => Ok(None),
    }
}

async fn run_tty(p: Arc<dyn Provider>, cred: Secret, o: Opts) -> i32 {
    let (cols, rows) = term_size();
    let mut session = match try_resume(&*p, &cred, &o, cols, rows).await {
        Ok(Some(s)) => s,
        Ok(None) => {
            let req = ExecReq {
                argv: o.cmd.clone(),
                env: o.env.clone(),
                cwd: o.workdir.clone(),
                tty: true,
                cols,
                rows,
                detachable: true,
            };
            match p.exec(&cred, &o.box_id, req).await {
                Ok(s) => {
                    if let Some(f) = &o.session_file
                        && !s.id.is_empty()
                    {
                        write_session_file(f, &s.id);
                    }
                    s
                }
                Err(e) => return fail(&o.provider, &e),
            }
        }
        Err(e) => return fail(&o.provider, &e),
    };
    let sid = session.id.clone();
    let _raw = RawMode::enter();
    let mut stdin = Some(stdin_reader());
    let mut stdout = tokio::io::stdout();
    let mut stderr = tokio::io::stderr();
    let (Ok(mut winch), Ok(mut term), Ok(mut hup)) = (
        signal(SignalKind::window_change()),
        signal(SignalKind::terminate()),
        signal(SignalKind::hangup()),
    ) else {
        eprintln!("vibeke: can't install signal handlers");
        return EXIT_CANT_RUN;
    };
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
            match p.attach(&cred, &o.box_id, &sid, cols, rows).await {
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
}
