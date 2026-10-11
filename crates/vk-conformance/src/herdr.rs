//! The Herdr side of a run (18 §10.2, §11): the pinned reference binary in its own temp HOME,
//! XDG, TMPDIR and runtime dirs, its TUI in a [`crate::pty::PtyDriver`], its socket behind a
//! recording proxy, and the crop rules that cut a plugin pane out of Herdr's screen.
//!
//! Source references are to Herdr at [`TAG_COMMIT`] (`src/…:line`). Nothing here was checked
//! against a running Herdr; see "To confirm" below.
//!
//! ## How Herdr starts
//!
//! * `herdr server` runs the headless server (`src/main.rs:560-561`). The default launch
//!   (`herdr` with no arguments) attaches the TUI client to a running server, or starts one
//!   (`src/server/autodetect.rs:295-327`); it needs a terminal with a size.
//! * The JSON API socket is `HERDR_SOCKET_PATH` when set (`src/api/mod.rs:20`,
//!   `src/session.rs:173-181`), else `<config dir>/herdr.sock`; the config dir is
//!   `$XDG_CONFIG_HOME/herdr` (`src/config/io.rs:30-35`). The client socket is derived from it
//!   by adding `-client` before `.sock` (`src/server/socket_paths.rs:35-58`).
//! * The wire format is one JSON object per line, `{"id","method","params"}` in and
//!   `{"id","result":{"type",…}}` or `{"id","error":{"code","message"}}` out
//!   (`src/api/schema.rs:36-40`, `src/api/schema/response.rs:24-40`, `src/session.rs:371-396`).
//!   `crates/vk-compat/src/herdr/wire.rs` describes the same format.
//! * Readiness: `ping` answers `{"type":"pong"}` (`src/api/schema/response.rs:45`); the TUI is
//!   ready when `pane.layout` answers and the screen has settled.
//!
//! ## The recording proxy
//!
//! Herdr gives every plugin process `HERDR_SOCKET_PATH` = the server's own API socket path
//! (`src/app/api/plugins/runtime.rs:39-47`, `src/app/api/plugins/panes.rs:251-257`); there is
//! no separate setting for plugins. So once the server answers `ping`, the harness renames the
//! real socket to `herdr-real.sock` (a listening Unix socket keeps working after a rename) and
//! binds the proxy at the original path. Calls made before the rename (for example by a
//! startup command that runs at server start) are not recorded. The TUI client uses the
//! separate client socket and is not affected.
//!
//! The request does not carry the plugin id. The proxy reads `HERDR_PLUGIN_ID` from the
//! environment of the connecting process (its peer pid) or of its nearest ancestor that has
//! it. That works on Linux; macOS may withhold other processes' environments, and then the
//! line says `plugin = "unknown"`. Connections from the harness itself are not traced.
//!
//! ## Frames and crop rules
//!
//! [`HerdrSide::start`] writes a fixed `config.toml` so the frame is the same for every
//! pane: `pane_borders = "always"`, `pane_outer_borders = true`, `pane_gaps = true`,
//! `pane_scrollbars = false`, tab bar on top.
//!
//! * `pane.layout` rectangles are the panes' outer rectangles after the border pass
//!   (`src/app/api/panes.rs:2108-2160`, calling `apply_pane_chrome`), relative to the pane
//!   area, which starts at (0, 0) and has the size the client reported
//!   (`src/server/headless.rs:2986-2987`, `src/client/mod.rs:244-258,753`).
//! * With the fixed config every split or tab pane has all four borders
//!   (`src/ui/panes.rs:92-159`; `shows_borders` in `src/config/model.rs:891-899`) and no
//!   scrollbar gutter (`src/ui/panes.rs:36-47`), so its inner area is the rectangle minus one
//!   cell on each side (`pane_inner_rect`, `src/ui/panes.rs:49-55`).
//! * A zoomed pane fills the whole pane area with all four borders (`src/ui/panes.rs:218-228`,
//!   `286-293`); `pane.layout` still reports the split rectangle and `zoomed = true`, so
//!   [`HerdrSide::pane_rect`] returns the area for the focused pane of a zoomed tab.
//! * A plugin `overlay` pane is a split of the focused pane whose tab is then zoomed
//!   (`src/app/custom_commands.rs:440-510`), so it crops like a zoomed pane.
//! * A popup is not in `pane.layout` (its result has `pane: null`). Its outer rectangle is
//!   centred in the pane area (`src/popup_size.rs:50-90`) and framed with a plain box
//!   (`src/client/shell/composition.rs:562-582`); its terminal starts one cell in and is one
//!   column narrower than the frame's inside when that is wider than 4 (a reserved scrollbar
//!   column, `src/popup_size.rs:73-85`).
//! * The pane area sits right of the sidebar and below the tab bar
//!   (`src/client/shell/config.rs:360-421`), and the client draws pane rectangles shifted by
//!   that origin (`src/client/shell/composition.rs:238-251`). So on screen the area starts at
//!   (screen columns − area width, screen rows − area height).
//!
//! ## To confirm (18 §10.1, §15)
//!
//! Confirmed from source: the socket paths and wire format, the plugin environment, the
//! fixed-config border rule for split, tab and zoomed panes, overlay = zoomed split, popup
//! geometry. Guesses that milestone 2's prototype must confirm on a real run:
//!
//! * that renaming the live API socket does not disturb the server;
//! * the screen offset of the pane area (it assumes the left sidebar and top tab bar, and a
//!   screen wider than `mobile_width_threshold`, 64 columns);
//! * popup detection: the harness finds the popup by its box corners, centred in the pane
//!   area ([`find_popup`]);
//! * which edge `pane.resize` moves for each direction ([`HerdrSide::resize_pane`] checks
//!   and flips).

use crate::grid::{Grid, Rect};
use crate::pty::{PtyDriver, Spawn};
use crate::trace::TraceLine;
use anyhow::{Context, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const BASELINE: &str = "0.9.3";
pub const TAG_COMMIT: &str = "7b116c05bfda646af39d2524c54e70c751f57ee8";

/// The fixed Herdr configuration that makes frames predictable (see the module docs).
pub const CONFIG_TOML: &str = r#"onboarding = false

[update]
version_check = false
manifest_check = false

[ui]
pane_borders = "always"
pane_outer_borders = true
pane_gaps = true
pane_scrollbars = false
tab_bar_position = "top"
hide_tab_bar_when_single_tab = false
"#;

const READY_TIMEOUT: Duration = Duration::from_secs(30);
const CALL_TIMEOUT: Duration = Duration::from_secs(30);
const CLI_TIMEOUT: Duration = Duration::from_secs(120);

fn sha256_file(path: &Path) -> anyhow::Result<String> {
    let mut f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 16];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Verify the binary's sha256 against `expected` BEFORE running it at all, then check that
/// `--version` reports [`BASELINE`]. Never resolves `herdr` from PATH.
pub fn verify_binary(bin: &Path, expected_sha256: &str) -> anyhow::Result<()> {
    ensure!(
        bin.is_absolute(),
        "the Herdr binary must be an absolute path, got {}",
        bin.display()
    );
    let want = expected_sha256.trim().to_ascii_lowercase();
    ensure!(
        want.len() == 64 && want.bytes().all(|b| b.is_ascii_hexdigit()),
        "expected sha256 {expected_sha256:?} is not 64 hex digits"
    );
    let got = sha256_file(bin)?;
    if got != want {
        bail!(
            "{} has sha256 {got}, expected {want}; not running it",
            bin.display()
        );
    }
    let home = std::env::temp_dir().join(format!(
        "vk-herdr-version-{}-{}",
        std::process::id(),
        now_ms()
    ));
    fs::create_dir(&home).with_context(|| format!("create {}", home.display()))?;
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
    let run = || {
        Command::new(bin)
            .arg("--version")
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &home)
            .current_dir(&home)
            .stdin(Stdio::null())
            .output()
    };
    // A freshly written binary can be briefly busy (ETXTBSY) while another thread forks.
    let mut out = run();
    for _ in 0..10 {
        if !matches!(&out, Err(e) if e.raw_os_error() == Some(libc::ETXTBSY)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
        out = run();
    }
    let _ = fs::remove_dir_all(&home);
    let out = out.with_context(|| format!("run {} --version", bin.display()))?;
    ensure!(
        out.status.success(),
        "{} --version failed: {}",
        bin.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let want_line = format!("herdr {BASELINE}");
    ensure!(
        stdout.trim() == want_line,
        "{} --version printed {:?}, expected {want_line:?}",
        bin.display(),
        stdout.trim()
    );
    Ok(())
}

pub struct HerdrSide {
    bin: PathBuf,
    root: PathBuf,
    home: PathBuf,
    env: Vec<(String, String)>,
    cols: u16,
    rows: u16,
    /// Where Herdr and its plugins think the API socket is; the proxy listens here.
    socket: PathBuf,
    /// The server's real socket after the rename.
    real_socket: PathBuf,
    server: Child,
    tui: Option<PtyDriver>,
    proxy: Option<Proxy>,
    next_id: AtomicU64,
}

pub struct HerdrOptions {
    pub bin: PathBuf,
    /// Root for HOME, XDG_*, TMPDIR and the runtime dir; created fresh by the caller.
    pub root: PathBuf,
    /// The outer terminal size of Herdr's TUI.
    pub cols: u16,
    pub rows: u16,
    /// Trace file the recording proxy appends [`crate::trace::TraceLine`]s to.
    pub trace: PathBuf,
    /// Extra environment for plugin processes (stubs), already allowlisted.
    pub env: Vec<(String, String)>,
}

/// One request on a Herdr socket; returns the whole response object.
fn request(socket: &Path, id: &str, method: &str, params: &Value) -> anyhow::Result<Value> {
    let mut s =
        UnixStream::connect(socket).with_context(|| format!("connect {}", socket.display()))?;
    s.set_read_timeout(Some(CALL_TIMEOUT))?;
    s.set_write_timeout(Some(CALL_TIMEOUT))?;
    let mut line = serde_json::to_vec(&json!({"id": id, "method": method, "params": params}))?;
    line.push(b'\n');
    s.write_all(&line)?;
    let mut reply = String::new();
    BufReader::new(&s)
        .read_line(&mut reply)
        .with_context(|| format!("read the {method} reply"))?;
    ensure!(!reply.is_empty(), "Herdr closed the connection on {method}");
    serde_json::from_str(&reply).with_context(|| format!("bad {method} reply {reply:?}"))
}

/// The `result` of a response, or its error as an `Err`.
fn result_of(method: &str, v: Value) -> anyhow::Result<Value> {
    if let Some(e) = v.get("error") {
        bail!(
            "{method}: {}: {}",
            e.get("code").and_then(Value::as_str).unwrap_or("?"),
            e.get("message").and_then(Value::as_str).unwrap_or("")
        );
    }
    v.get("result")
        .cloned()
        .with_context(|| format!("{method}: reply has neither result nor error: {v}"))
}

/// The tail of a log file, for error messages.
fn log_tail(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap_or_default();
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// The parts of a `pane.layout` result the harness uses.
struct Layout {
    workspace: String,
    tab: String,
    zoomed: bool,
    area: Rect,
    focused: String,
    panes: Vec<(String, Rect)>,
}

fn rect_of(v: &Value) -> anyhow::Result<Rect> {
    let n = |k: &str| -> anyhow::Result<u16> {
        v.get(k)
            .and_then(Value::as_u64)
            .and_then(|n| u16::try_from(n).ok())
            .with_context(|| format!("pane.layout rect: bad `{k}` in {v}"))
    };
    Ok(Rect {
        row: n("y")?,
        col: n("x")?,
        rows: n("height")?,
        cols: n("width")?,
    })
}

fn parse_layout(result: &Value) -> anyhow::Result<Layout> {
    let l = result
        .get("layout")
        .with_context(|| format!("pane.layout: no `layout` in {result}"))?;
    let panes = l
        .get("panes")
        .and_then(Value::as_array)
        .context("pane.layout: no `panes`")?
        .iter()
        .map(|p| {
            let id = p
                .get("pane_id")
                .and_then(Value::as_str)
                .context("pane.layout: pane without `pane_id`")?;
            Ok((
                id.to_string(),
                rect_of(p.get("rect").unwrap_or(&Value::Null))?,
            ))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let text = |k: &str| {
        l.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    Ok(Layout {
        workspace: text("workspace_id"),
        tab: text("tab_id"),
        zoomed: l.get("zoomed").and_then(Value::as_bool).unwrap_or(false),
        area: rect_of(l.get("area").unwrap_or(&Value::Null))?,
        focused: l
            .get("focused_pane_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        panes,
    })
}

impl Layout {
    /// The pane's outer rectangle relative to the pane area (see the module docs).
    fn rect_of(&self, pane: &str) -> anyhow::Result<Rect> {
        if self.zoomed && self.focused == pane {
            return Ok(self.area);
        }
        self.panes
            .iter()
            .find(|(id, _)| id == pane)
            .map(|(_, r)| *r)
            .with_context(|| format!("pane {pane} is not in pane.layout"))
    }
}

impl HerdrSide {
    pub fn start(opts: HerdrOptions) -> anyhow::Result<HerdrSide> {
        ensure!(opts.bin.is_absolute(), "the Herdr binary must be absolute");
        let root = &opts.root;
        let dir = |name: &str| -> anyhow::Result<PathBuf> {
            let d = root.join(name);
            fs::create_dir_all(&d).with_context(|| format!("create {}", d.display()))?;
            fs::set_permissions(&d, fs::Permissions::from_mode(0o700))?;
            Ok(d)
        };
        let home = dir("home")?;
        let config = dir("config")?;
        let data = dir("data")?;
        let state = dir("state")?;
        let cache = dir("cache")?;
        let run = dir("run")?;
        let tmp = dir("tmp")?;
        let socket = run.join("herdr.sock");
        let real_socket = run.join("herdr-real.sock");
        // The longest socket path Herdr derives is the client socket (macOS limit: 104 bytes).
        let client_socket = run.join("herdr-client.sock");
        ensure!(
            client_socket.as_os_str().len() < 100,
            "socket path {} is too long for a Unix socket; use a shorter root",
            client_socket.display()
        );
        let app_config = config.join("herdr");
        fs::create_dir_all(&app_config)?;
        fs::write(app_config.join("config.toml"), CONFIG_TOML)?;

        let bin_dir = opts
            .bin
            .parent()
            .context("the Herdr binary has no parent directory")?;
        let path_of = |p: &Path| p.display().to_string();
        let mut env: Vec<(String, String)> = vec![
            (
                "PATH".into(),
                format!("{}:/usr/bin:/bin:/usr/sbin:/sbin", bin_dir.display()),
            ),
            ("HOME".into(), path_of(&home)),
            ("XDG_CONFIG_HOME".into(), path_of(&config)),
            ("XDG_DATA_HOME".into(), path_of(&data)),
            ("XDG_STATE_HOME".into(), path_of(&state)),
            ("XDG_CACHE_HOME".into(), path_of(&cache)),
            ("XDG_RUNTIME_DIR".into(), path_of(&run)),
            ("TMPDIR".into(), path_of(&tmp)),
            ("TERM".into(), "xterm-256color".into()),
            ("LANG".into(), "C.UTF-8".into()),
        ];
        for (k, v) in &opts.env {
            if k == "HERDR_SOCKET_PATH" {
                continue;
            }
            env.retain(|(ek, _)| ek != k);
            env.push((k.clone(), v.clone()));
        }
        env.push(("HERDR_SOCKET_PATH".into(), path_of(&socket)));

        // The server, in its own session and process group, logging to a file.
        let log_path = root.join("herdr-server.log");
        let log = File::create(&log_path)?;
        let mut cmd = Command::new(&opts.bin);
        cmd.arg("server")
            .current_dir(&home)
            .env_clear()
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log.try_clone()?))
            .stderr(Stdio::from(log));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut server = cmd
            .spawn()
            .with_context(|| format!("start {} server", opts.bin.display()))?;

        let deadline = Instant::now() + READY_TIMEOUT;
        loop {
            if let Some(status) = server.try_wait()? {
                bail!(
                    "herdr server exited early ({status}); log:\n{}",
                    log_tail(&log_path)
                );
            }
            let pong = request(&socket, "ready", "ping", &json!({}))
                .ok()
                .and_then(|v| {
                    v.pointer("/result/type")
                        .and_then(Value::as_str)
                        .map(String::from)
                });
            if pong.as_deref() == Some("pong") {
                break;
            }
            if Instant::now() >= deadline {
                kill_group(&mut server);
                bail!(
                    "herdr server did not answer ping within {READY_TIMEOUT:?}; log:\n{}",
                    log_tail(&log_path)
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let mut side = HerdrSide {
            bin: opts.bin.clone(),
            root: opts.root.clone(),
            home: home.clone(),
            env: env.clone(),
            cols: opts.cols,
            rows: opts.rows,
            socket: socket.clone(),
            real_socket: real_socket.clone(),
            server,
            tui: None,
            proxy: None,
            next_id: AtomicU64::new(1),
        };
        // From here on, `side` cleans up through `stop` on error.
        if let Err(e) = side.finish_start(&opts.trace) {
            let _ = side.stop();
            return Err(e);
        }
        Ok(side)
    }

    fn finish_start(&mut self, trace: &Path) -> anyhow::Result<()> {
        fs::rename(&self.socket, &self.real_socket).context("move Herdr's socket aside")?;
        self.proxy = Some(Proxy::start(&self.socket, &self.real_socket, trace)?);

        let tui = PtyDriver::spawn(&Spawn {
            program: &self.bin,
            args: &[],
            cwd: &self.home,
            env: &self.env,
            cols: self.cols,
            rows: self.rows,
        })?;
        self.tui = Some(tui);

        // Ready: pane.layout answers for a foreground client, and the screen has settled.
        let deadline = Instant::now() + READY_TIMEOUT;
        let mut last = String::new();
        let mut since = Instant::now();
        loop {
            let (rows, cols, nrows, _) = self.tui()?.snapshot();
            let text = Grid::from_rows(&rows, cols, nrows).text();
            if text != last {
                last = text;
                since = Instant::now();
            }
            let settled = !last.trim().is_empty() && since.elapsed() >= Duration::from_millis(500);
            if settled
                && let Ok(l) = self.layout(None)
                && l.area.cols > 0
                && l.area.cols <= self.cols
                && l.area.rows <= self.rows
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!(
                    "Herdr's TUI was not ready within {READY_TIMEOUT:?}; screen:\n{last}\nserver log:\n{}",
                    log_tail(&self.root.join("herdr-server.log"))
                );
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn tui(&self) -> anyhow::Result<&PtyDriver> {
        self.tui.as_ref().context("Herdr's TUI is not running")
    }

    /// Run the Herdr CLI (`herdr <args>`) against this side, returning (exit code, stdout,
    /// stderr). It sees the same environment as Herdr, so its calls go through the proxy
    /// (traced with `plugin = "unknown"`).
    pub fn cli(&self, args: &[&str]) -> anyhow::Result<(i32, String, String)> {
        self.cli_with(args, &[], CLI_TIMEOUT)
    }

    /// [`HerdrSide::cli`] with extra environment for this one command and its own time limit
    /// (`herdr plugin install` runs the plugin's build commands, which can take many minutes).
    pub fn cli_with(
        &self,
        args: &[&str],
        extra_env: &[(String, String)],
        timeout: Duration,
    ) -> anyhow::Result<(i32, String, String)> {
        let out_path = self.root.join("cli.out");
        let err_path = self.root.join("cli.err");
        let mut child = Command::new(&self.bin)
            .args(args)
            .current_dir(&self.home)
            .env_clear()
            .envs(self.env.iter().map(|(k, v)| (k, v)))
            .envs(extra_env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .stdout(Stdio::from(File::create(&out_path)?))
            .stderr(Stdio::from(File::create(&err_path)?))
            .spawn()
            .with_context(|| format!("run herdr {args:?}"))?;
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("herdr {args:?} did not finish within {timeout:?}");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        let read =
            |p: &Path| String::from_utf8_lossy(&fs::read(p).unwrap_or_default()).into_owned();
        Ok((
            status.code().unwrap_or(-1),
            read(&out_path),
            read(&err_path),
        ))
    }

    /// One JSON request on Herdr's socket through the proxy. The harness's own connections
    /// are not traced.
    pub fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> anyhow::Result<serde_json::Value> {
        let id = format!("harness-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        result_of(method, request(&self.socket, &id, method, &params)?)
    }

    /// The active workspace, tab and focused pane of Herdr's TUI client, from `pane.layout`.
    pub fn active(&self) -> anyhow::Result<(String, String, String)> {
        let l = self.layout(None)?;
        ensure!(!l.focused.is_empty(), "pane.layout reports no focused pane");
        Ok((l.workspace, l.tab, l.focused))
    }

    /// The text on Herdr's TUI screen right now.
    pub fn screen_text(&self) -> anyhow::Result<String> {
        let (rows, cols, nrows, _) = self.tui()?.snapshot();
        Ok(Grid::from_rows(&rows, cols, nrows).text())
    }

    /// The server's log file (stdout and stderr of `herdr server`).
    pub fn server_log(&self) -> PathBuf {
        self.root.join("herdr-server.log")
    }

    /// HOME of Herdr and its plugin processes.
    pub fn home(&self) -> &Path {
        &self.home
    }

    fn layout(&self, pane: Option<&str>) -> anyhow::Result<Layout> {
        let params = match pane {
            Some(p) => json!({ "pane_id": p }),
            None => json!({}),
        };
        parse_layout(&self.call("pane.layout", params)?)
    }

    /// The plugin pane's rectangle from `pane.layout`, including its frame, relative to
    /// Herdr's pane area (for a zoomed or overlay pane: the whole area).
    pub fn pane_rect(&self, pane: &str) -> anyhow::Result<Rect> {
        self.layout(Some(pane))?.rect_of(pane)
    }

    /// Capture Herdr's screen and crop the plugin pane's inner area: `pane.layout` rectangle
    /// minus the frame, with separate rules for zoomed panes, overlays and popups. For a
    /// popup, `pane` is not used for the geometry (popups are not in `pane.layout`).
    pub fn capture_pane(&self, pane: &str, placement: &str) -> anyhow::Result<Grid> {
        let (rows, cols, nrows, palette) = self.tui()?.snapshot();
        let mut screen = Grid::from_rows(&rows, cols, nrows);
        screen.resolve_palette(&palette);
        let popup = placement == "popup";
        let layout = self.layout((!popup).then_some(pane))?;
        let area = Rect {
            row: nrows.saturating_sub(layout.area.rows) + layout.area.row,
            col: cols.saturating_sub(layout.area.cols) + layout.area.col,
            rows: layout.area.rows,
            cols: layout.area.cols,
        };
        let outer = if popup {
            find_popup(&screen, area).context("no popup frame centred in Herdr's pane area")?
        } else {
            let r = layout.rect_of(pane)?;
            Rect {
                row: area.row - layout.area.row + r.row,
                col: area.col - layout.area.col + r.col,
                ..r
            }
        };
        Ok(screen.crop(crop_rect(outer, placement)))
    }

    /// Resize a split pane until its inner size is `cols`×`rows`. Herdr's `pane.resize` only
    /// moves a split by a fraction of the area in a direction (`src/api/schema/panes.rs:229-236`,
    /// `src/app/api/panes.rs:713-765`, default 0.05, at most 0.5), so this steps toward the
    /// target and checks the result. When the size cannot be reached (a lone, zoomed or popup
    /// pane, or rounding), it returns an error and the step compares placement only
    /// (18 §10.2).
    pub fn resize_pane(&self, pane: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        let inner =
            |l: &Layout| -> anyhow::Result<Rect> { Ok(crop_rect(l.rect_of(pane)?, "split-right")) };
        let mut l = self.layout(Some(pane))?;
        ensure!(
            !l.zoomed,
            "pane {pane} is zoomed; Herdr cannot resize it; comparing placement only (18 §10.2)"
        );
        // Whether "grow" means `right`/`down` (false) or `left`/`up` (true), per axis.
        let mut flip = [false, false];
        let mut stuck = 0;
        for _ in 0..32 {
            let now = inner(&l)?;
            if now.cols == cols && now.rows == rows {
                return Ok(());
            }
            let (axis, have, want, extent) = if now.cols != cols {
                (0, now.cols, cols, l.area.cols)
            } else {
                (1, now.rows, rows, l.area.rows)
            };
            let grow = want > have;
            let forward = grow != flip[axis];
            let direction = match (axis, forward) {
                (0, true) => "right",
                (0, false) => "left",
                (_, true) => "down",
                (_, false) => "up",
            };
            let delta = want.abs_diff(have) as f64;
            let amount = (delta / extent.max(1) as f64).clamp(0.005, 0.5);
            let r = self.call(
                "pane.resize",
                json!({"pane_id": pane, "direction": direction, "amount": amount}),
            )?;
            let changed = r
                .pointer("/resize/changed")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            l = self.layout(Some(pane))?;
            let after = inner(&l)?;
            let got = if axis == 0 { after.cols } else { after.rows };
            if !changed || got == have {
                stuck += 1;
                flip[axis] = !flip[axis];
                if stuck >= 4 {
                    break;
                }
                continue;
            }
            stuck = 0;
            if got.abs_diff(want) > have.abs_diff(want) {
                // That direction moved the wrong edge for this pane.
                flip[axis] = !flip[axis];
            }
        }
        let now = inner(&l)?;
        bail!(
            "could not resize Herdr pane {pane} to {cols}x{rows} (now {}x{}); comparing placement only (18 §10.2)",
            now.cols,
            now.rows
        )
    }

    pub fn stop(mut self) -> anyhow::Result<()> {
        // Ask the server to stop on its real socket, then make sure.
        let _ = request(&self.real_socket, "stop", "server.stop", &json!({}));
        if let Some(tui) = self.tui.take() {
            let _ = tui.shutdown(Duration::from_secs(2));
        }
        if let Some(p) = self.proxy.take() {
            p.stop();
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.server.try_wait()?.is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        kill_group(&mut self.server);
        Ok(())
    }
}

/// Git settings for `herdr plugin install <owner/repo> --ref <commit>` that make it fetch from
/// the local bare mirror `mirror_repo` instead of GitHub. Herdr fetches
/// `https://github.com/<owner>/<repo>.git` (`src/cli/plugin.rs`, `remote_url`); the rewrite
/// covers exactly that URL, so other fetches of the plugin's build commands are unchanged. Key
/// 0 trusts the mirror, which another user owns.
pub fn git_mirror_env(mirror_repo: &Path, repository: &str) -> Vec<(String, String)> {
    let pairs = [
        ("safe.directory".to_string(), "*".to_string()),
        (
            format!("url.file://{}.insteadOf", mirror_repo.display()),
            format!("https://github.com/{repository}.git"),
        ),
    ];
    let mut env = vec![("GIT_CONFIG_COUNT".to_string(), pairs.len().to_string())];
    for (i, (k, v)) in pairs.into_iter().enumerate() {
        env.push((format!("GIT_CONFIG_KEY_{i}"), k));
        env.push((format!("GIT_CONFIG_VALUE_{i}"), v));
    }
    env
}

/// SIGKILL the process group led by `child` (it ran `setsid`) and reap it.
fn kill_group(child: &mut Child) {
    // SAFETY: kill has no memory-safety preconditions.
    unsafe { libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL) };
    let _ = child.wait();
}

/// The crop rule for one placement, given the `pane.layout` rectangle (frame included).
/// For `zoomed` and `overlay`, `layout` is the whole pane area ([`HerdrSide::pane_rect`]);
/// for `popup`, the popup's outer frame. See the module docs for the sources.
pub fn crop_rect(layout: Rect, placement: &str) -> Rect {
    let inner = Rect {
        row: layout.row.saturating_add(1),
        col: layout.col.saturating_add(1),
        rows: layout.rows.saturating_sub(2),
        cols: layout.cols.saturating_sub(2),
    };
    match placement {
        // `src/popup_size.rs:73-85`: one column is kept for a scrollbar when the frame's
        // inside is wider than 4.
        "popup" => Rect {
            cols: if inner.cols <= 4 {
                inner.cols
            } else {
                inner.cols - 1
            },
            ..inner
        },
        // split-right, split-down, tab, zoomed, overlay: all four borders with the fixed
        // config (`src/ui/panes.rs:49-55,128-155,218-228`).
        _ => inner,
    }
}

/// Find a popup frame (`┌┐└┘` corners) centred in `area` the way
/// `src/popup_size.rs:71-72` centres it. A full-area frame is a pane frame, not a popup; of
/// several candidates the smallest wins (the popup is drawn last, on top of pane frames).
pub fn find_popup(screen: &Grid, area: Rect) -> Option<Rect> {
    let at = |row: u16, col: u16| -> &str {
        screen
            .cells
            .get(row as usize)
            .and_then(|l| l.get(col as usize))
            .map(|c| c.text.as_str())
            .unwrap_or("")
    };
    let mut best: Option<Rect> = None;
    let row_end = area.row.saturating_add(area.rows).min(screen.rows);
    let col_end = area.col.saturating_add(area.cols).min(screen.cols);
    for y in area.row..row_end {
        for x in area.col..col_end {
            if at(y, x) != "┌" {
                continue;
            }
            for x2 in (x + 5)..col_end {
                if at(y, x2) != "┐" {
                    continue;
                }
                let w = x2 - x + 1;
                if x != area.col + (area.cols - w) / 2 {
                    continue;
                }
                for y2 in (y + 3)..row_end {
                    if at(y2, x) != "└" || at(y2, x2) != "┘" {
                        continue;
                    }
                    let h = y2 - y + 1;
                    if y != area.row + (area.rows - h) / 2 || (w == area.cols && h == area.rows) {
                        continue;
                    }
                    let r = Rect {
                        row: y,
                        col: x,
                        rows: h,
                        cols: w,
                    };
                    if best.is_none_or(|b| (b.rows as u32 * b.cols as u32) > (h as u32 * w as u32))
                    {
                        best = Some(r);
                    }
                }
            }
        }
    }
    best
}

/// The recording proxy (see the module docs).
struct Proxy {
    stop: Arc<AtomicBool>,
    path: PathBuf,
    thread: JoinHandle<()>,
}

impl Proxy {
    fn start(path: &Path, upstream: &Path, trace: &Path) -> anyhow::Result<Proxy> {
        let listener =
            UnixListener::bind(path).with_context(|| format!("bind proxy {}", path.display()))?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        let trace = Arc::new(Mutex::new(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(trace)
                .with_context(|| format!("open trace {}", trace.display()))?,
        ));
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (stop, upstream) = (stop.clone(), upstream.to_path_buf());
            std::thread::Builder::new()
                .name("herdr-proxy".into())
                .spawn(move || {
                    for conn in listener.incoming() {
                        if stop.load(Ordering::SeqCst) {
                            return;
                        }
                        let Ok(conn) = conn else { continue };
                        let (upstream, trace) = (upstream.clone(), trace.clone());
                        let _ = std::thread::Builder::new()
                            .name("herdr-proxy-conn".into())
                            .spawn(move || proxy_connection(conn, &upstream, trace));
                    }
                })?
        };
        Ok(Proxy {
            stop,
            path: path.to_path_buf(),
            thread,
        })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        // Wake the accept loop.
        let _ = UnixStream::connect(&self.path);
        let _ = self.thread.join();
    }
}

/// The pid of the process at the other end of a Unix socket.
fn peer_pid(s: &UnixStream) -> Option<u32> {
    #[cfg(target_os = "linux")]
    {
        rustix::net::sockopt::socket_peercred(s)
            .ok()
            .map(|c| rustix::process::Pid::as_raw(Some(c.pid)) as u32)
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SOL_LOCAL = 0, LOCAL_PEERPID = 2 (<sys/un.h>).
        let mut pid: libc::pid_t = 0;
        let mut len = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        // SAFETY: getsockopt writes at most `len` bytes into `pid`.
        let rc = unsafe {
            libc::getsockopt(
                s.as_raw_fd(),
                0,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut len,
            )
        };
        (rc == 0 && pid > 0).then_some(pid as u32)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = s;
        None
    }
}

/// `HERDR_PLUGIN_ID` from `pid`'s environment or its nearest ancestor's (at most 8 levels).
fn plugin_of(pid: u32) -> Option<String> {
    let mut p = pid;
    for _ in 0..8 {
        if let Some((_, v)) = vk_hold::procinfo::environ(p)
            .into_iter()
            .find(|(k, _)| k == "HERDR_PLUGIN_ID")
        {
            return Some(v);
        }
        let parent = vk_hold::procinfo::info(p)?.ppid;
        if parent <= 1 || parent == p {
            return None;
        }
        p = parent;
    }
    None
}

/// Forward one connection line by line in both directions; pair each response with its
/// request by `id` and append a trace line for it.
fn proxy_connection(client: UnixStream, upstream: &Path, trace: Arc<Mutex<File>>) {
    let pid = peer_pid(&client);
    // The harness's own calls are not plugin calls.
    let traced = pid != Some(std::process::id());
    let plugin = pid
        .and_then(plugin_of)
        .unwrap_or_else(|| "unknown".to_string());
    let Ok(server) = UnixStream::connect(upstream) else {
        return;
    };
    let (Ok(client_w), Ok(server_r)) = (client.try_clone(), server.try_clone()) else {
        return;
    };
    let pending: Arc<Mutex<HashMap<String, String>>> = Arc::default();

    // Server to client, on its own thread.
    let back = {
        let pending = pending.clone();
        std::thread::spawn(move || {
            let mut r = BufReader::new(server_r);
            let mut w = client_w;
            let mut line = Vec::new();
            loop {
                line.clear();
                match r.read_until(b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                if traced
                    && let Ok(v) = serde_json::from_slice::<Value>(&line)
                    && let Some(id) = v.get("id").and_then(Value::as_str)
                    && let Some(method) = pending.lock().unwrap().remove(id)
                {
                    let code = v
                        .pointer("/error/code")
                        .and_then(Value::as_str)
                        .map(String::from);
                    let t = TraceLine {
                        ts_ms: now_ms(),
                        plugin: plugin.clone(),
                        invocation: None,
                        transport: "socket".into(),
                        method,
                        ok: v.get("result").is_some() && code.is_none(),
                        code,
                        params: None,
                        argv: None,
                        parse_error: None,
                    };
                    if let Ok(mut s) = serde_json::to_string(&t) {
                        s.push('\n');
                        let _ = trace.lock().unwrap().write_all(s.as_bytes());
                    }
                }
                if w.write_all(&line).is_err() {
                    break;
                }
            }
            let _ = w.shutdown(std::net::Shutdown::Write);
        })
    };

    // Client to server on this thread.
    let mut r = BufReader::new(client);
    let mut w = server;
    let mut line = Vec::new();
    loop {
        line.clear();
        match r.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if traced
            && let Ok(v) = serde_json::from_slice::<Value>(&line)
            && let (Some(id), Some(method)) = (
                v.get("id").and_then(Value::as_str),
                v.get("method").and_then(Value::as_str),
            )
        {
            pending
                .lock()
                .unwrap()
                .insert(id.to_string(), method.to_string());
        }
        if w.write_all(&line).is_err() {
            break;
        }
    }
    let _ = w.shutdown(std::net::Shutdown::Write);
    let _ = back.join();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(row: u16, col: u16, rows: u16, cols: u16) -> Rect {
        Rect {
            row,
            col,
            rows,
            cols,
        }
    }

    #[test]
    fn crop_rules() {
        assert_eq!(
            crop_rect(rect(0, 40, 24, 40), "split-right"),
            rect(1, 41, 22, 38)
        );
        assert_eq!(crop_rect(rect(0, 0, 30, 100), "zoomed"), rect(1, 1, 28, 98));
        // Popup: one column kept for the scrollbar.
        assert_eq!(crop_rect(rect(7, 25, 15, 50), "popup"), rect(8, 26, 13, 47));
        assert_eq!(crop_rect(rect(0, 0, 4, 6), "popup"), rect(1, 1, 2, 4));
    }

    #[test]
    fn layout_parsing_and_zoom() {
        let result = json!({"type": "pane_layout", "layout": {
            "workspace_id": "w1", "tab_id": "t1", "zoomed": true,
            "area": {"x": 0, "y": 0, "width": 100, "height": 30},
            "focused_pane_id": "p2",
            "panes": [
                {"pane_id": "p1", "focused": false, "rect": {"x": 0, "y": 0, "width": 50, "height": 30}},
                {"pane_id": "p2", "focused": true, "rect": {"x": 50, "y": 0, "width": 50, "height": 30}}
            ],
            "splits": []
        }});
        let l = parse_layout(&result).unwrap();
        assert_eq!((l.workspace.as_str(), l.tab.as_str()), ("w1", "t1"));
        assert_eq!(l.rect_of("p2").unwrap(), rect(0, 0, 30, 100));
        assert_eq!(l.rect_of("p1").unwrap(), rect(0, 0, 30, 50));
        assert!(l.rect_of("p9").is_err());
    }

    #[test]
    fn git_mirror_env_rewrites_only_the_plugin_repository() {
        let env = git_mirror_env(Path::new("/opt/hp/mirror/acme/tools"), "acme/tools");
        let get = |k: &str| {
            env.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert_eq!(get("GIT_CONFIG_COUNT"), "2");
        assert_eq!(get("GIT_CONFIG_KEY_0"), "safe.directory");
        assert_eq!(
            get("GIT_CONFIG_KEY_1"),
            "url.file:///opt/hp/mirror/acme/tools.insteadOf"
        );
        assert_eq!(
            get("GIT_CONFIG_VALUE_1"),
            "https://github.com/acme/tools.git"
        );
    }

    #[test]
    fn finds_centred_popup_not_pane_frame() {
        // A 20x8 pane area at (1, 2) on a 22x9 screen: a full-area pane frame and a centred
        // 10x4 popup.
        let mut g = Grid::blank(22, 9);
        let mut put = |r: u16, c: u16, s: &str| g.cells[r as usize][c as usize].text = s.into();
        let area = rect(1, 2, 8, 20);
        for (r, c, rows, cols) in [(1, 2, 8, 20), (3, 7, 4, 10)] {
            put(r, c, "┌");
            put(r, c + cols - 1, "┐");
            put(r + rows - 1, c, "└");
            put(r + rows - 1, c + cols - 1, "┘");
        }
        assert_eq!(find_popup(&g, area), Some(rect(3, 7, 4, 10)));
        assert_eq!(find_popup(&Grid::blank(22, 9), area), None);
    }

    #[test]
    fn verify_binary_checks_sha_before_running() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("herdr");
        // A script that would leave a marker if it ran.
        let marker = dir.path().join("ran");
        fs::write(
            &bin,
            format!("#!/bin/sh\ntouch {}\necho herdr 0.9.3\n", marker.display()),
        )
        .unwrap();
        fs::set_permissions(&bin, fs::Permissions::from_mode(0o755)).unwrap();
        let err = verify_binary(&bin, &"0".repeat(64))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not running it"), "{err}");
        assert!(!marker.exists());
        assert!(verify_binary(Path::new("herdr"), &"0".repeat(64)).is_err());
        let sha = sha256_file(&bin).unwrap();
        verify_binary(&bin, &sha.to_uppercase()).unwrap();
        assert!(marker.exists());
    }
}
