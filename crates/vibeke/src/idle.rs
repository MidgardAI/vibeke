//! `vibeke debug idle`: resource usage of an idle session against spec 10 §1.3.
//!
//! Starts an isolated server (private runtime/state dirs under /tmp), creates K idle panes
//! (2/3 `cat` standing in for agents waiting at a prompt, 1/3 `sh` shells), then samples CPU,
//! RSS and wakeups of the server, the holders and (second phase) an attached headless TUI client
//! over `--seconds` windows, `--repeat` times. Everything it starts is killed on exit (Drop).
//!
//! CPU/RSS/wakeups come from `proc_pid_rusage` (macOS: `ri_interrupt_wkups`, `ri_pkg_idle_wkups`)
//! and `/proc` (Linux: voluntary context switches as the wakeup proxy). Numbers are only
//! meaningful on an otherwise quiet machine: the report carries the load average and marks
//! verdicts "(loaded)" when load1 exceeds half the cores.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vk_term::Engine;

#[derive(Clone, Copy, Default, Debug)]
struct Sample {
    cpu_s: f64,
    rss: u64,
    /// Interrupt wakeups (macOS ri_interrupt_wkups; Linux voluntary context switches).
    wake: u64,
    /// Package-idle wakeups (macOS only).
    wake_pkg: u64,
}

#[cfg(target_os = "macos")]
// libc points at mach2 for the timebase; one call does not warrant a dependency.
#[allow(deprecated)]
fn sample(pid: u32) -> Option<Sample> {
    let mut ri: libc::rusage_info_v4 = unsafe { std::mem::zeroed() };
    // The C prototype takes `rusage_info_t *` = the address of the struct, cast.
    let rc = unsafe {
        libc::proc_pid_rusage(
            pid as i32,
            libc::RUSAGE_INFO_V4,
            (&raw mut ri).cast::<libc::rusage_info_t>(),
        )
    };
    if rc != 0 {
        return None;
    }
    let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
    unsafe { libc::mach_timebase_info(&mut tb) };
    let ns =
        (ri.ri_user_time + ri.ri_system_time) as f64 * tb.numer as f64 / tb.denom.max(1) as f64;
    Some(Sample {
        cpu_s: ns / 1e9,
        rss: ri.ri_resident_size,
        wake: ri.ri_interrupt_wkups,
        wake_pkg: ri.ri_pkg_idle_wkups,
    })
}

#[cfg(target_os = "linux")]
fn sample(pid: u32) -> Option<Sample> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // Fields after the parenthesised comm: state is field 3; utime/stime are 14/15.
    let rest = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = rest.split_whitespace().collect();
    let tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) } as f64;
    let cpu_s = (f.get(11)?.parse::<f64>().ok()? + f.get(12)?.parse::<f64>().ok()?) / tck;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let kb = |key: &str| -> u64 {
        status
            .lines()
            .find_map(|l| l.strip_prefix(key))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0)
    };
    let mut wake = 0;
    if let Ok(rd) = std::fs::read_dir(format!("/proc/{pid}/task")) {
        for t in rd.flatten() {
            if let Ok(s) = std::fs::read_to_string(t.path().join("status")) {
                wake += s
                    .lines()
                    .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
                    .and_then(|v| v.trim().parse::<u64>().ok())
                    .unwrap_or(0);
            }
        }
    }
    Some(Sample {
        cpu_s,
        rss: kb("VmRSS:") * 1024,
        wake,
        wake_pkg: 0,
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn sample(_pid: u32) -> Option<Sample> {
    None
}

fn load1() -> f64 {
    let mut l = [0f64; 3];
    unsafe { libc::getloadavg(l.as_mut_ptr(), 3) };
    l[0]
}

fn ncpu() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Rates over one window for a group of pids.
#[derive(Clone, Copy, Default, Debug)]
struct Rates {
    cpu_pct: f64,
    wake_s: f64,
    wake_pkg_s: f64,
    rss: u64,
}

fn rates(a: &[Option<Sample>], b: &[Option<Sample>], secs: f64) -> Rates {
    let mut r = Rates::default();
    for (x, y) in a.iter().zip(b) {
        if let (Some(x), Some(y)) = (x, y) {
            r.cpu_pct += (y.cpu_s - x.cpu_s) / secs * 100.0;
            r.wake_s += y.wake.saturating_sub(x.wake) as f64 / secs;
            r.wake_pkg_s += y.wake_pkg.saturating_sub(x.wake_pkg) as f64 / secs;
            r.rss += y.rss;
        }
    }
    r
}

fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    let mut v = v.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

struct Env {
    dir: PathBuf,
    exe: PathBuf,
}

impl Env {
    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(&self.exe);
        c.env("VIBEKE_RUNTIME_DIR", self.dir.join("run"))
            .env("VIBEKE_STATE_DIR", self.dir.join("state"))
            .env("VIBEKE_CONFIG", self.dir.join("config.toml"))
            .env("VIBEKE_NO_OPEN", "1");
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
        ] {
            c.env_remove(k);
        }
        c.arg("--json")
            .args(args)
            .stdin(Stdio::null())
            .stderr(Stdio::null());
        c
    }
    fn api(&self, method: &str, params: Value) -> Option<Value> {
        let out = self
            .cmd(&["api", "call", method, &params.to_string()])
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| serde_json::from_slice(&out.stdout).ok())
            .flatten()
    }
}

/// Everything started by the run; killed on drop whatever the exit path.
struct Run {
    env: Env,
    server: Option<u32>,
    tui: Option<std::process::Child>,
}

impl Run {
    fn holders(&self) -> Vec<u32> {
        let needle = self.env.dir.to_string_lossy().into_owned();
        let out = Command::new("ps")
            .args(["-axo", "pid=,command="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        out.lines()
            .filter(|l| l.contains(" hold ") && l.contains("--spec") && l.contains(&needle))
            .filter_map(|l| l.split_whitespace().next()?.parse().ok())
            .collect()
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(mut t) = self.tui.take() {
            let _ = t.kill();
            let _ = t.wait();
        }
        let holders = self.holders();
        let _ = self.env.api("server.stop", json!({}));
        std::thread::sleep(Duration::from_millis(300));
        let mut victims = holders;
        victims.extend(self.server);
        let mut all = Vec::new();
        for p in &victims {
            for i in vk_hold::procinfo::tree(*p, 4) {
                all.push(i.pid);
            }
        }
        for p in all.into_iter().chain(victims) {
            unsafe { libc::kill(p as i32, libc::SIGKILL) };
        }
        let _ = std::fs::remove_dir_all(&self.env.dir);
    }
}

static STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, std::sync::atomic::Ordering::Relaxed);
}

fn stopped() -> bool {
    STOP.load(std::sync::atomic::Ordering::Relaxed)
}

/// Sleep that returns early on SIGINT/SIGTERM so `Run::drop` still cleans up.
fn nap(d: Duration) {
    let end = Instant::now() + d;
    while Instant::now() < end && !stopped() {
        std::thread::sleep(Duration::from_millis(100).min(end - Instant::now()));
    }
}

fn mib(b: u64) -> f64 {
    b as f64 / 1048576.0
}

struct Row {
    metric: String,
    measured: String,
    budget: &'static str,
    verdict: String,
}

pub fn idle(args: &[String]) -> i32 {
    let get = |k: &str| {
        args.iter()
            .position(|a| a == k)
            .and_then(|i| args.get(i + 1))
            .cloned()
    };
    let seconds: u64 = get("--seconds").and_then(|v| v.parse().ok()).unwrap_or(20);
    let repeat: usize = get("--repeat").and_then(|v| v.parse().ok()).unwrap_or(3);
    let panes: usize = get("--panes").and_then(|v| v.parse().ok()).unwrap_or(30);
    let json_out = args.iter().any(|a| a == "--json");
    let exe = std::env::current_exe().unwrap_or_else(|_| "vibeke".into());
    let dir = PathBuf::from(format!("/tmp/vkidle-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut run = Run {
        env: Env { dir, exe },
        server: None,
        tui: None,
    };
    macro_rules! bail {
        ($($t:tt)*) => {{ eprintln!($($t)*); return 1; }};
    }
    let load_start = load1();
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
    }

    // Server with no panes: baseline RSS.
    let Some(st) = run.env.api("server.status", json!({})) else {
        bail!("could not start an isolated server");
    };
    let Some(spid) = st["pid"].as_u64().map(|p| p as u32) else {
        bail!("server.status returned no pid: {st}");
    };
    run.server = Some(spid);
    nap(Duration::from_secs(2));
    let base_rss = sample(spid).map_or(0, |s| s.rss);

    // K idle panes: one workspace each (2/3 `cat` = agent waiting for input, 1/3 `sh` shells).
    for i in 0..panes {
        let cmd: &[&str] = if i % 3 == 2 {
            &["/bin/sh"]
        } else {
            &["/bin/cat"]
        };
        if stopped() {
            bail!("interrupted");
        }
        if run
            .env
            .api(
                "workspace.create",
                json!({"cwd": "/tmp", "name": format!("idle-{i}"), "command": cmd}),
            )
            .is_none()
        {
            bail!("workspace.create {i} failed");
        }
    }
    let holders = run.holders();
    if holders.len() < panes {
        eprintln!("warning: found {} holders for {panes} panes", holders.len());
    }
    nap(Duration::from_secs(3)); // settle
    let window = Duration::from_secs(seconds);
    let secs = seconds as f64;
    let procs = |run: &Run, holders: &[u32]| -> Vec<Option<Sample>> {
        let mut v = vec![run.server.and_then(sample)];
        v.extend(holders.iter().map(|h| sample(*h)));
        v
    };

    // Phase A: no client attached.
    let (mut a_srv_cpu, mut a_srv_wake, mut a_srv_pkg, mut a_hold_cpu, mut a_hold_wake) =
        (vec![], vec![], vec![], vec![], vec![]);
    let mut a_srv_rss = 0;
    let mut a_hold_rss = 0;
    for _ in 0..repeat {
        let s0 = procs(&run, &holders);
        let t0 = Instant::now();
        nap(window);
        if stopped() {
            bail!("interrupted");
        }
        let el = t0.elapsed().as_secs_f64().max(secs);
        let s1 = procs(&run, &holders);
        let srv = rates(&s0[..1], &s1[..1], el);
        let hol = rates(&s0[1..], &s1[1..], el);
        a_srv_cpu.push(srv.cpu_pct);
        a_srv_wake.push(srv.wake_s);
        a_srv_pkg.push(srv.wake_pkg_s);
        a_hold_cpu.push(hol.cpu_pct);
        a_hold_wake.push(hol.wake_s);
        a_srv_rss = srv.rss;
        a_hold_rss = hol.rss;
    }

    // Phase B: headless TUI attached (it answers terminal queries like a host would).
    let d = &run.env.dir;
    let envv: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| !k.starts_with("VIBEKE"))
        .chain([
            ("TERM".into(), "xterm-256color".into()),
            (
                "VIBEKE_RUNTIME_DIR".into(),
                d.join("run").to_string_lossy().into_owned(),
            ),
            (
                "VIBEKE_STATE_DIR".into(),
                d.join("state").to_string_lossy().into_owned(),
            ),
            (
                "VIBEKE_CONFIG".into(),
                d.join("config.toml").to_string_lossy().into_owned(),
            ),
        ])
        .collect();
    let argv = vec![run.env.exe.to_string_lossy().into_owned(), "attach".into()];
    let (pty, child) = match vk_hold::pty::spawn(&argv, &PathBuf::from("/tmp"), &envv, 120, 36) {
        Ok(x) => x,
        Err(e) => bail!("spawn tui: {e:#}"),
    };
    let tui_pid = child.id();
    run.tui = Some(child);
    let mut engine = Engine::new(120, 36, 100);
    let mut fx = Vec::new();
    let mut buf = [0u8; 65536];
    let mut pump_for = |d: Duration, engine: &mut Engine| {
        let end = Instant::now() + d;
        while Instant::now() < end && !stopped() {
            match rustix::io::read(&pty.master, &mut buf) {
                Ok(n) if n > 0 => {
                    engine.feed(&buf[..n], &mut fx);
                    for e in fx.drain(..) {
                        if let vk_term::Effect::Reply(b) = e {
                            let _ = rustix::io::write(&pty.master, &b);
                        }
                    }
                }
                _ => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    };
    pump_for(Duration::from_secs(4), &mut engine);
    let drawn = engine.screen_text().trim().len() > 20;
    let (mut b_srv_cpu, mut b_tui_cpu, mut b_srv_wake, mut b_tui_wake) =
        (vec![], vec![], vec![], vec![]);
    let mut tui_rss = 0;
    for _ in 0..repeat {
        let s0 = (sample(spid), sample(tui_pid));
        let t0 = Instant::now();
        pump_for(window, &mut engine);
        if stopped() {
            bail!("interrupted");
        }
        let el = t0.elapsed().as_secs_f64().max(secs);
        let s1 = (sample(spid), sample(tui_pid));
        let srv = rates(&[s0.0], &[s1.0], el);
        let tui = rates(&[s0.1], &[s1.1], el);
        b_srv_cpu.push(srv.cpu_pct);
        b_srv_wake.push(srv.wake_s);
        b_tui_cpu.push(tui.cpu_pct);
        b_tui_wake.push(tui.wake_s);
        tui_rss = tui.rss;
    }
    let final_srv_rss = sample(spid).map_or(a_srv_rss, |s| s.rss);

    let load_end = load1();
    let loaded = load_start.max(load_end) > ncpu() as f64 / 2.0;
    let suffix = if loaded { " (loaded)" } else { "" };
    let verdict = |ok: bool| format!("{}{suffix}", if ok { "PASS" } else { "FAIL" });
    let (srv_cpu, srv_wake, srv_pkg) =
        (median(&a_srv_cpu), median(&a_srv_wake), median(&a_srv_pkg));
    let (hold_cpu, hold_wake) = (median(&a_hold_cpu), median(&a_hold_wake));
    let (bs_cpu, bt_cpu) = (median(&b_srv_cpu), median(&b_tui_cpu));
    let (bs_wake, bt_wake) = (median(&b_srv_wake), median(&b_tui_wake));
    let per_pane = a_srv_rss.saturating_sub(base_rss) as f64 / panes.max(1) as f64;
    let hold_avg = a_hold_rss as f64 / holders.len().max(1) as f64;
    let rows = vec![
        Row {
            metric: format!("server idle CPU, {panes} panes, no client"),
            measured: format!("{srv_cpu:.2}% of a core"),
            budget: "<= 0.3%",
            verdict: verdict(srv_cpu <= 0.3),
        },
        Row {
            metric: "server idle wakeups, no client".into(),
            measured: format!("{srv_wake:.1}/s (pkg-idle {srv_pkg:.1}/s)"),
            budget: "<= 2/s",
            verdict: verdict(srv_wake <= 2.0),
        },
        Row {
            metric: "holder idle RSS (mean)".into(),
            measured: format!("{:.2} MiB", hold_avg / 1048576.0),
            budget: "<= 2 MiB + ring",
            verdict: verdict(hold_avg <= 2.0 * 1048576.0),
        },
        Row {
            metric: "server RSS baseline, no panes".into(),
            measured: format!("{:.1} MiB", mib(base_rss)),
            budget: "<= 25 MiB",
            verdict: verdict(mib(base_rss) <= 25.0),
        },
        Row {
            metric: "server RSS per idle pane".into(),
            measured: format!("{:.2} MiB", per_pane / 1048576.0),
            budget: "<= 6 MiB (with 10k scrollback)",
            verdict: verdict(per_pane <= 6.0 * 1048576.0),
        },
        Row {
            metric: "TUI client RSS".into(),
            measured: format!("{:.1} MiB", mib(tui_rss)),
            budget: "<= 30 MiB",
            verdict: verdict(mib(tui_rss) <= 30.0),
        },
        Row {
            metric: "server + TUI CPU, TUI attached, panes idle".into(),
            measured: format!(
                "{:.2}% (server {bs_cpu:.2}, tui {bt_cpu:.2})",
                bs_cpu + bt_cpu
            ),
            budget: "<= 1% (spinner budgets not exercised)",
            verdict: verdict(bs_cpu + bt_cpu <= 1.0),
        },
        Row {
            metric: "holders CPU, all, no client".into(),
            measured: format!("{hold_cpu:.2}% total, {hold_wake:.1} wakeups/s total"),
            budget: "informational",
            verdict: "REVIEW".into(),
        },
        Row {
            metric: "TUI idle wakeups".into(),
            measured: format!("{bt_wake:.1}/s (server {bs_wake:.1}/s)"),
            budget: "informational",
            verdict: "REVIEW".into(),
        },
    ];
    let host = Command::new("uname")
        .arg("-a")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let report = json!({
        "host": host, "ncpu": ncpu(), "load1_start": load_start, "load1_end": load_end,
        "loaded": loaded, "panes": panes, "holders_found": holders.len(),
        "window_s": seconds, "repeats": repeat, "tui_drew_a_frame": drawn,
        "wakeup_source": if cfg!(target_os = "macos") { "proc_pid_rusage ri_interrupt_wkups" } else { "voluntary_ctxt_switches" },
        "samples": {
            "server_cpu_pct_no_client": a_srv_cpu, "server_wake_s_no_client": a_srv_wake,
            "server_cpu_pct_attached": b_srv_cpu, "tui_cpu_pct_attached": b_tui_cpu,
        },
        "server_rss_end_bytes": final_srv_rss,
        "rows": rows.iter().map(|r| json!({"metric": r.metric, "measured": r.measured, "budget": r.budget, "verdict": r.verdict})).collect::<Vec<_>>(),
    });
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        println!(
            "idle resource report ({panes} panes, {repeat} x {seconds}s windows, {} cores, load1 {load_start:.2} -> {load_end:.2}{})",
            ncpu(),
            if loaded {
                ", HOST LOADED: indicative only"
            } else {
                ""
            }
        );
        if !drawn {
            println!("warning: the headless TUI drew no recognisable frame");
        }
        for r in &rows {
            println!(
                "{:<46} {:<46} {:<32} {}",
                r.metric, r.measured, r.budget, r.verdict
            );
        }
        println!(
            "per-window server CPU % no client: {}",
            report["samples"]["server_cpu_pct_no_client"]
        );
    }
    0
}
