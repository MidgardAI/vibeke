//! Native TUI keystroke-to-visible-marker latency in a disposable session. The host PTY is
//! interpreted by the same VT engine used for panes, including replies to terminal queries.
//! This includes TUI input, composition and output pacing, but not physical display latency.

use super::{Env, Run, STOP, load1, ncpu, on_signal, stopped};
use anyhow::{Context, Result, bail, ensure};
use rustix::fd::{AsFd, AsRawFd, BorrowedFd};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use vk_term::Engine;

const HELP: &str = "vibeke debug tui-latency [--n COUNT] [--binary PATH] [--json]\n\
Measure native TUI key-to-screen latency with an isolated server and pane.\n\
--binary selects the server and TUI executable (default: this executable).\n\
The observer and echo helper always use this executable for comparison.\n\
COUNT is 1..10000 (default 100). Ten warm-up samples are excluded.\n\
Keys have a 20 ms gap, excluded from the measured latency.\n\
The host terminal is emulated. Physical display latency is not measured.";
const READY: &str = "VK_TUI_LATENCY_READY";
const WARMUP: usize = 10;
const SAMPLE_GAP: Duration = Duration::from_millis(20);
const COLS: u16 = 120;
const ROWS: u16 = 36;

pub fn run(args: &[String], json_out: bool) -> i32 {
    // Internal pane process: each received byte produces one unique marker. It bypasses
    // shell startup, line discipline echo and external commands in the measured path.
    if args == ["--echo"] {
        return match echo() {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("tui latency echo: {error:#}");
                1
            }
        };
    }
    if args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--help" | "-h"))
    {
        println!("{HELP}");
        return 0;
    }
    let options = match parse_options(args) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("{error:#}\n{HELP}");
            return vk_cli::EXIT_USAGE;
        }
    };
    let n = options.n;
    match measure(&options) {
        Ok(report) => {
            if json_out {
                println!("{}", serde_json::to_string_pretty(&report).unwrap());
            } else {
                println!(
                    "Native TUI key-to-screen latency, {n} samples ({COLS}x{ROWS}, emulated host)"
                );
                println!("measured binary: {}", options.binary.display());
                let ms = &report["latency_ms"];
                println!(
                    "p50 {:.3} ms  p95 {:.3} ms  p99 {:.3} ms",
                    ms["p50"].as_f64().unwrap(),
                    ms["p95"].as_f64().unwrap(),
                    ms["p99"].as_f64().unwrap(),
                );
                println!("render counter deltas: {}", report["render_counter_deltas"]);
                println!(
                    "load1 {:.2} -> {:.2} ({} cores); physical display latency is excluded",
                    report["load1_start"].as_f64().unwrap(),
                    report["load1_end"].as_f64().unwrap(),
                    report["ncpu"],
                );
            }
            0
        }
        Err(error) => {
            eprintln!("tui latency: {error:#}");
            1
        }
    }
}

#[derive(Debug)]
struct Options {
    n: usize,
    binary: PathBuf,
}

fn parse_options(args: &[String]) -> Result<Options> {
    let mut n = 100;
    let mut binary = std::env::current_exe()?;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--n" => {
                n = args
                    .next()
                    .context("--n needs a count")?
                    .parse()
                    .context("--n needs an integer")?;
                ensure!((1..=10000).contains(&n), "--n must be 1..10000");
            }
            "--binary" => binary = args.next().context("--binary needs a path")?.into(),
            _ => bail!("unknown argument {arg}"),
        }
    }
    Ok(Options {
        n,
        binary: executable_path(&binary)?,
    })
}

fn executable_path(path: &Path) -> Result<PathBuf> {
    let path = path
        .canonicalize()
        .with_context(|| format!("resolve binary {}", path.display()))?;
    let metadata = path.metadata()?;
    ensure!(
        metadata.is_file(),
        "binary is not a file: {}",
        path.display()
    );
    ensure!(
        metadata.permissions().mode() & 0o111 != 0,
        "binary is not executable: {}",
        path.display()
    );
    Ok(path)
}

fn measure(options: &Options) -> Result<Value> {
    let n = options.n;
    // Keep the observer and pane helper identical while comparing different TUI builds.
    let observer = std::env::current_exe()?.canonicalize()?;
    // A short, private directory also keeps UNIX sockets below macOS's 104-byte limit.
    let dir = tempfile::Builder::new()
        .prefix("vktui-")
        .tempdir_in("/tmp")?;
    let mut run = Run {
        env: Env {
            dir: dir.path().to_path_buf(),
            exe: options.binary.clone(),
        },
        server: None,
        tui: None,
    };
    std::fs::write(
        run.env.dir.join("config.toml"),
        "onboarding = false\n[config]\nwatch = false\n",
    )?;
    STOP.store(false, Ordering::Relaxed);
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }
    let load_start = load1();
    let status = run
        .env
        .api("server.status", json!({}))
        .context("could not start isolated server")?;
    run.server = Some(
        status["pid"]
            .as_u64()
            .context("server.status returned no pid")? as u32,
    );
    ensure!(!stopped(), "interrupted");
    run.env
        .api(
            "workspace.create",
            json!({
                "cwd": run.env.dir,
                "name": "tui-latency",
                "command": [observer, "debug", "tui-latency", "--echo"],
            }),
        )
        .context("could not create isolated probe pane")?;
    let argv = vec![run.env.exe.to_string_lossy().into_owned(), "attach".into()];
    let (pty, child) = vk_hold::pty::spawn(&argv, &run.env.dir, &run.env.vars(), COLS, ROWS)
        .context("spawn native TUI")?;
    run.tui = Some(child);
    let mut host = Engine::new(COLS, ROWS, 0);
    let fd = pty.master.as_fd();
    wait_for_marker(
        fd,
        &mut host,
        READY,
        Instant::now() + Duration::from_secs(15),
    )
    .context("wait for native TUI and probe pane")?;

    for i in 1..=WARMUP {
        one_sample(fd, &mut host, i)?;
    }
    let before = run
        .env
        .api("server.status", json!({}))
        .context("read initial render counters")?;
    let mut samples = Vec::with_capacity(n);
    for i in WARMUP + 1..=WARMUP + n {
        samples.push(one_sample(fd, &mut host, i)?.as_secs_f64() * 1000.0);
    }
    let after = run
        .env
        .api("server.status", json!({}))
        .context("read final render counters")?;
    let load_end = load1();
    let mut sorted = samples.clone();
    sorted.sort_by(f64::total_cmp);
    let percentile = |p: usize| sorted[(n * p).div_ceil(100).saturating_sub(1)];
    Ok(json!({
        "measurement": "native_tui_key_to_host_vt_marker",
        "measured_binary": options.binary,
        "observer_binary": observer,
        "observer_version": vk_proto::VERSION,
        "n": n,
        "warmup_samples": WARMUP,
        "sample_gap_ms": SAMPLE_GAP.as_millis(),
        "terminal": {"cols": COLS, "rows": ROWS},
        "physical_display_included": false,
        "latency_ms": {"p50": percentile(50), "p95": percentile(95), "p99": percentile(99)},
        "samples_ms": samples,
        "render_counter_deltas": counter_deltas(&before["render"], &after["render"]),
        "ncpu": ncpu(), "load1_start": load_start, "load1_end": load_end,
        "loaded": load_start.max(load_end) > ncpu() as f64 / 2.0,
    }))
}

fn counter_deltas(before: &Value, after: &Value) -> Value {
    let mut delta = serde_json::Map::new();
    for key in [
        "session_wakes",
        "timer_wakes",
        "row_projections",
        "pane_frames",
    ] {
        if let (Some(a), Some(b)) = (before[key].as_u64(), after[key].as_u64()) {
            delta.insert(key.into(), json!(b.saturating_sub(a)));
        }
    }
    Value::Object(delta)
}

fn marker(n: usize) -> String {
    format!("VK_TUI_LATENCY_{n:08}")
}

fn one_sample(fd: BorrowedFd<'_>, host: &mut Engine, n: usize) -> Result<Duration> {
    // Let the last frame budget expire. Back-to-back input would measure throughput and
    // miss the extra frame interval caused by a no-op paint after an otherwise idle key.
    pump_host_until(fd, host, None, Instant::now() + SAMPLE_GAP)?;
    let expected = marker(n);
    let start = Instant::now();
    let deadline = start + Duration::from_secs(5);
    write_pty(fd, b"x", deadline)?;
    wait_for_marker(fd, host, &expected, deadline).with_context(|| format!("sample {n}"))?;
    Ok(start.elapsed())
}

fn wait_for_marker(
    fd: BorrowedFd<'_>,
    host: &mut Engine,
    marker: &str,
    deadline: Instant,
) -> Result<()> {
    ensure!(
        pump_host_until(fd, host, Some(marker), deadline)?,
        "timed out waiting for visible marker"
    );
    Ok(())
}

/// Service terminal queries and output until the marker appears or the deadline expires.
/// A missing marker means a pacing interval: output is still consumed while poll waits.
fn pump_host_until(
    fd: BorrowedFd<'_>,
    host: &mut Engine,
    marker: Option<&str>,
    deadline: Instant,
) -> Result<bool> {
    let mut bytes = [0u8; 65536];
    let mut effects = Vec::new();
    while poll_pty(fd, libc::POLLIN, deadline)? {
        match rustix::io::read(fd, &mut bytes) {
            Ok(0) => bail!("TUI PTY closed before measurement completed"),
            Ok(n) => {
                host.feed(&bytes[..n], &mut effects);
                for effect in effects.drain(..) {
                    if let vk_term::Effect::Reply(reply) = effect {
                        write_pty(fd, &reply, Instant::now() + Duration::from_secs(1))?;
                    }
                }
                // A synchronized update has not reached the host's visible screen yet.
                if let Some(marker) = marker
                    && !host.in_sync_update()
                    && host.screen_text().lines().any(|row| row.contains(marker))
                {
                    return Ok(true);
                }
            }
            Err(rustix::io::Errno::AGAIN | rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error).context("read TUI PTY"),
        }
    }
    Ok(false)
}

fn write_pty(fd: BorrowedFd<'_>, mut bytes: &[u8], deadline: Instant) -> Result<()> {
    while !bytes.is_empty() {
        ensure!(!stopped(), "interrupted");
        ensure!(Instant::now() < deadline, "timed out writing to TUI PTY");
        match rustix::io::write(fd, bytes) {
            Ok(0) => bail!("TUI PTY closed while writing"),
            Ok(n) => bytes = &bytes[n..],
            Err(rustix::io::Errno::AGAIN) => ensure!(
                poll_pty(fd, libc::POLLOUT, deadline)?,
                "timed out writing to TUI PTY"
            ),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error).context("write TUI PTY"),
        }
    }
    Ok(())
}

fn poll_pty(fd: BorrowedFd<'_>, events: libc::c_short, deadline: Instant) -> Result<bool> {
    loop {
        ensure!(!stopped(), "interrupted");
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events,
            revents: 0,
        };
        // The cap permits prompt signal cleanup even on platforms with restarted syscalls.
        let timeout = left.as_millis().clamp(1, 250) as i32;
        let result = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if result < 0 {
            let error = std::io::Error::last_os_error();
            if error.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error).context("poll TUI PTY");
        }
        ensure!(pfd.revents & libc::POLLNVAL == 0, "invalid TUI PTY");
        if result > 0 && pfd.revents & (events | libc::POLLHUP | libc::POLLERR) != 0 {
            return Ok(true);
        }
    }
}

fn echo() -> Result<()> {
    let mut termios: libc::termios = unsafe { std::mem::zeroed() };
    ensure!(
        unsafe { libc::tcgetattr(libc::STDIN_FILENO, &mut termios) } == 0,
        "probe pane needs a PTY"
    );
    unsafe { libc::cfmakeraw(&mut termios) };
    ensure!(
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios) } == 0,
        "could not set probe pane raw mode"
    );
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    write!(stdout, "\x1b[2J\x1b[H{READY}")?;
    stdout.flush()?;
    let mut key = [0u8; 1];
    let mut n = 0;
    while stdin.read(&mut key)? != 0 {
        n += 1;
        write!(stdout, "\r\x1b[2K{}", marker(n))?;
        stdout.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| (*arg).to_string()).collect()
    }

    #[test]
    fn options_bound_sample_count_and_require_values() {
        let default = parse_options(&[]).unwrap();
        assert_eq!(default.n, 100);
        assert_eq!(
            default.binary,
            std::env::current_exe().unwrap().canonicalize().unwrap()
        );
        assert_eq!(parse_options(&args(&["--n", "1"])).unwrap().n, 1);
        assert_eq!(parse_options(&args(&["--n", "10000"])).unwrap().n, 10000);
        for invalid in [
            vec!["--n"],
            vec!["--n", "0"],
            vec!["--n", "10001"],
            vec!["--n", "-1"],
            vec!["--n", "1.5"],
            vec!["--binary"],
            vec!["--unknown"],
        ] {
            assert!(parse_options(&args(&invalid)).is_err(), "{invalid:?}");
        }
    }

    #[test]
    fn binary_path_is_canonical_and_requires_an_executable_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("probe binary");
        let command = vec!["--binary".to_string(), path.to_string_lossy().into_owned()];
        assert!(parse_options(&command).is_err(), "missing file");
        assert!(executable_path(directory.path()).is_err(), "directory");
        std::fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(parse_options(&command).is_err(), "non-executable file");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(
            parse_options(&command).unwrap().binary,
            path.canonicalize().unwrap()
        );
    }
}
