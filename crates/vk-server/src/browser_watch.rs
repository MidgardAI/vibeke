//! Watch mode for browser panes (06 B7): a browser pane bound to an **agent browser session**
//! instead of a page of its own.
//!
//! - `browser.watch {session | agent_pane, pane?, split?, focus?}` puts a browser pane with
//!   `BrowserPane.watch = Some(<session>)` into the layout of the machine that owns the session
//!   (next to the agent's pane by default). The TUI reports a watch pane's view to that machine
//!   (the pane's own server, never the laptop's media host), so for a remote agent the frames
//!   cross the existing render stream over the bridge as zlib tiles.
//! - **Frames**: while the pane is visible the pump holds an
//!   [`AgentBrowsers::attach_screencast`](crate::agent_browser::AgentBrowsers::attach_screencast)
//!   subscription (latest-wins JPEG frames), decodes each frame, scales it to fit the pane's
//!   content pixels (aspect kept) and feeds it into the ordinary tile pipeline
//!   ([`Target::on_frame`]): cell-aligned tile diff, per-subscriber dirty sets, media channel.
//!   Hidden → the subscription is dropped and the agent's screencast stops.
//! - **Read-only** by default: keys, text, mouse and wheel are ignored (with a hint).
//! - **Take over** (`BrowserCmd::TakeOver(true)`, `prefix+t` in the TUI) sets the session's
//!   human control (`browser.taken_over`; the agent's calls fail with `human_control`) and from
//!   then on the pane's input goes through `AgentBrowsers::human_input`, with mouse
//!   coordinates mapped from the scaled frame back to the agent's viewport. **Release**
//!   (`TakeOver(false)`, `prefix+t` again) or closing the pane clears it.

use super::{Geom, Target};
use crate::Server;
use crate::agent_browser::{self, WatchInfo};
use crate::api::{Ctx, R, b, err, invalid, not_found, resolve_pane, s};
use serde_json::{Value, json};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};
use vk_browser::frame::{Rgba, decode, scale_to_fit};
use vk_browser::input::{self, CdpInput, MapOptions};
use vk_proto::input::{KeyKind, MouseButton, MouseKind};
use vk_proto::render::BrowserCmd;
use vk_proto::rpc::ErrorKind;

/// How often the pump re-checks the session (closed, taken over elsewhere) and visibility
/// when no frame arrives.
const POLL: Duration = Duration::from_millis(300);
/// Read-only hints are shown at most this often.
const HINT_EVERY: Duration = Duration::from_secs(4);

/// Watch state of one target (inside `TState`).
#[derive(Debug, Clone)]
pub struct Watch {
    /// The session handle as requested (`b3`).
    pub session: String,
    pub pump: bool,
    /// Someone holds human control of the session.
    pub human: bool,
    /// This pane holds it (input is forwarded).
    pub here: bool,
    /// Pane device px → agent viewport CSS px (per device pixel of the pane).
    pub to_page: f64,
    pub last_hint: Option<Instant>,
}

impl Watch {
    pub fn new(session: String) -> Watch {
        Watch {
            session,
            pump: false,
            human: false,
            here: false,
            to_page: 1.0,
            last_hint: None,
        }
    }
}

pub fn env_label(server: &Server, session: &str) -> String {
    format!("agent session {session} · {}", server.opts.machine)
}

fn holder(t: &Target) -> String {
    format!("pane:{}", t.pane)
}

// ---- creating ---------------------------------------------------------------------------------

/// `browser.watch {session | agent_pane, pane?, split?, focus?}` (human callers only).
pub fn create(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    if ctx.pane_scope.is_some() {
        return Err(err(
            ErrorKind::PermissionDenied,
            "browser.watch is not allowed from a pane (human-only action)",
        )
        .details(json!({"scope": "pane"})));
    }
    let session = match s(p, "session") {
        Some(x) => x.to_string(),
        None => {
            let ap = s(p, "agent_pane")
                .ok_or_else(|| invalid("browser.watch needs `session` or `agent_pane`"))?;
            let pane = resolve_pane(server, ctx, Some(ap))?;
            server
                .agent_browser
                .session_of_pane(&pane.id)
                .ok_or_else(|| {
                    err(
                        ErrorKind::NotFound,
                        format!("{} has no open browser session", pane.handle),
                    )
                    .details(json!({"object": "browser_session", "pane": pane.id}))
                })?
        }
    };
    let info: WatchInfo = server
        .agent_browser
        .watch_info(&session)
        .ok_or_else(|| not_found("browser_session", &session))?;
    let source = match s(p, "pane") {
        Some(x) => Some(resolve_pane(server, ctx, Some(x))?.id),
        None => info
            .owner_pane
            .clone()
            .filter(|x| server.with_core(|c| c.pane(x).is_some())),
    };
    let mut params = json!({
        "watch": info.handle,
        "url": info.url,
        "split": s(p, "split").unwrap_or("right"),
        "focus": b(p, "focus").unwrap_or(true),
    });
    if let Some(src) = source {
        params["pane"] = json!(src);
    }
    if let Some(fc) = s(p, "focus_client") {
        params["focus_client"] = json!(fc);
    }
    let mut r = super::create_pane(server, ctx, &params)?;
    r["opened_in"] = json!("watch");
    r["session"] = json!(info.handle);
    r["read_only"] = json!(true);
    Ok(r)
}

// ---- frames -----------------------------------------------------------------------------------

/// Start the frame pump for a watch target (once).
pub fn ensure_pump(server: &Arc<Server>, t: &Arc<Target>) {
    {
        let mut st = t.st.lock().unwrap();
        let Some(w) = st.watch.as_mut() else { return };
        if w.pump {
            return;
        }
        w.pump = true;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        return;
    }
    let weak = Arc::downgrade(server);
    let t = t.clone();
    tokio::spawn(pump(weak, t));
}

/// Pane content in device pixels (the box frames are scaled into).
fn content_px(g: Option<Geom>) -> (u32, u32, f32) {
    match g {
        Some(g) if g.cols > 0 && g.rows > 0 => (
            g.cols as u32 * g.cell_w.max(1) as u32,
            g.rows as u32 * g.cell_h.max(1) as u32,
            if g.dpr > 0.0 { g.dpr } else { 1.0 },
        ),
        _ => (0, 0, 1.0),
    }
}

/// Decode, scale to the pane and publish one agent frame. `css_w` is the agent viewport width
/// in CSS px (screencast metadata), used to map input back.
fn publish(t: &Target, raw: &Rgba, css_w: Option<u32>) {
    let geom = t.st.lock().unwrap().want;
    let (max_w, max_h, _) = content_px(geom);
    let img = scale_to_fit(raw, max_w, max_h);
    let px_per_css = css_w
        .filter(|w| *w > 0)
        .map(|w| raw.width as f64 / w as f64)
        .unwrap_or(1.0);
    // pane device px → raw frame px → agent CSS px
    let to_page = (raw.width as f64 / img.width.max(1) as f64) / px_per_css;
    {
        let mut st = t.st.lock().unwrap();
        if let Some(w) = st.watch.as_mut() {
            w.to_page = to_page;
        }
        st.css = (
            css_w.unwrap_or(raw.width),
            (raw.height as f64 / px_per_css).round() as u32,
        );
    }
    t.on_frame(img);
}

/// Session bookkeeping: closed / taken over / URL. Returns whether the session is alive.
fn refresh(server: &Server, t: &Target) -> bool {
    let session = match t.st.lock().unwrap().watch.as_ref() {
        Some(w) => w.session.clone(),
        None => return false,
    };
    let info = server.agent_browser.watch_info(&session);
    let mut st = t.st.lock().unwrap();
    let mine = holder(t);
    let changed = match &info {
        None => {
            let msg = format!("agent browser session {session} is closed");
            let was = st.error.as_deref() == Some(msg.as_str());
            st.error = Some(msg);
            if let Some(w) = st.watch.as_mut() {
                w.human = false;
                w.here = false;
            }
            st.loading = false;
            !was
        }
        Some(i) => {
            let human = i.human.is_some();
            let here = i.human.as_deref() == Some(mine.as_str());
            let w = st.watch.as_mut().expect("watch target");
            let mut changed = w.human != human || w.here != here;
            w.human = human;
            w.here = here;
            if st.url != i.url {
                st.url = i.url.clone();
                changed = true;
            }
            if st.error.is_some() || st.loading {
                st.error = None;
                st.loading = false;
                changed = true;
            }
            changed
        }
    };
    if changed {
        Target::mark_state(&mut st);
    }
    info.is_some()
}

async fn pump(weak: Weak<Server>, t: Arc<Target>) {
    let mut sub: Option<agent_browser::ScreencastSub> = None;
    let mut last: Option<(Rgba, Option<u32>)> = None;
    let mut last_geom: Option<Geom> = None;
    loop {
        let Some(server) = weak.upgrade() else { return };
        let (closed, viewers, session, geom) = {
            let st = t.st.lock().unwrap();
            (
                st.closed,
                !st.subs.is_empty(),
                st.watch.as_ref().map(|w| w.session.clone()),
                st.want,
            )
        };
        let Some(session) = session else { return };
        if closed {
            drop(sub);
            on_close(&server, &t);
            return;
        }
        let alive = refresh(&server, &t);
        if !viewers || !alive {
            // Frames stop when nobody looks (the agent's screencast stops with the last viewer).
            sub = None;
        } else if sub.is_none() {
            match server.agent_browser.attach_screencast(&session).await {
                Ok(s) => {
                    let cur = s.frames.borrow().clone();
                    sub = Some(s);
                    if let Some(f) = cur {
                        last = decode_frame(&f).await;
                        if let Some((raw, css_w)) = &last {
                            publish(&t, raw, *css_w);
                        }
                    }
                }
                Err(e) => {
                    let mut st = t.st.lock().unwrap();
                    st.error = Some(e.message.clone());
                    Target::mark_state(&mut st);
                }
            }
        }
        // Geometry changed while the page is idle: rescale the last frame.
        if geom != last_geom {
            last_geom = geom;
            if let Some((raw, css_w)) = &last
                && viewers
            {
                publish(&t, raw, *css_w);
            }
        }
        drop(server);
        match sub.as_mut() {
            Some(s) => {
                tokio::select! {
                    r = s.frames.changed() => {
                        if r.is_err() {
                            sub = None;
                            continue;
                        }
                        let f = s.frames.borrow_and_update().clone();
                        if let Some(f) = f {
                            last = decode_frame(&f).await;
                            if let Some((raw, css_w)) = &last {
                                publish(&t, raw, *css_w);
                            }
                        }
                    }
                    _ = tokio::time::sleep(POLL) => {}
                }
            }
            None => tokio::time::sleep(POLL).await,
        }
    }
}

async fn decode_frame(f: &agent_browser::Frame) -> Option<(Rgba, Option<u32>)> {
    let data = f.data.clone();
    let css_w = f.width;
    tokio::task::spawn_blocking(move || decode(&data).ok())
        .await
        .ok()
        .flatten()
        .map(|img| (img, css_w))
}

// ---- input and commands -------------------------------------------------------------------------

fn notice(t: &Target, msg: impl Into<String>) {
    let mut st = t.st.lock().unwrap();
    st.notice = Some(msg.into());
    Target::mark_state(&mut st);
}

/// A read-only hint, at most every few seconds.
fn hint(t: &Target, msg: &str) {
    let mut st = t.st.lock().unwrap();
    let Some(w) = st.watch.as_mut() else { return };
    if w.last_hint.is_some_and(|at| at.elapsed() < HINT_EVERY) {
        return;
    }
    w.last_hint = Some(Instant::now());
    st.notice = Some(msg.to_string());
    Target::mark_state(&mut st);
}

/// What a command does in watch mode (pure; the state machine the tests pin down).
#[derive(Debug, Clone, PartialEq)]
pub enum Decision {
    /// Take over (`true`) or release (`false`).
    Control(bool),
    /// Forward to the agent session as human input.
    Forward(Vec<CdpInput>),
    /// Ignored: read-only (not taken over here).
    ReadOnly,
    /// Ignored: no meaning while watching (navigation chrome, window, screenshot).
    Unsupported,
    /// Nothing to do (e.g. a key release on a host that synthesizes them).
    Nothing,
}

/// Decide what `cmd` does for a watch pane. `here` = this pane holds human control;
/// `to_page` maps pane device px to agent CSS px; `dpr` is the pane host's DPR.
pub fn decide(
    cmd: &BrowserCmd,
    here: bool,
    key_releases: bool,
    to_page: f64,
    dpr: f32,
) -> Decision {
    let page = |x: f32, y: f32| -> (f64, f64) {
        let k = to_page * dpr as f64;
        (x as f64 * k, y as f64 * k)
    };
    match cmd {
        BrowserCmd::TakeOver(on) => Decision::Control(*on),
        BrowserCmd::Key(_)
        | BrowserCmd::Text(_)
        | BrowserCmd::Mouse { .. }
        | BrowserCmd::Wheel { .. }
            if !here =>
        {
            Decision::ReadOnly
        }
        BrowserCmd::Key(ev) => {
            if !key_releases && ev.kind == KeyKind::Release {
                return Decision::Nothing;
            }
            let opts = MapOptions {
                host_reports_text: false,
                ..Default::default()
            };
            let v = if key_releases {
                input::map_key(ev, opts)
            } else {
                input::map_key_with_release(ev, opts)
            };
            if v.is_empty() {
                Decision::Nothing
            } else {
                Decision::Forward(v)
            }
        }
        BrowserCmd::Text(t) => Decision::Forward(vec![input::map_paste(t)]),
        BrowserCmd::Mouse {
            kind,
            button,
            x,
            y,
            mods,
            clicks,
        } => {
            let (px, py) = page(*x, *y);
            let ty = match kind {
                MouseKind::Press => "mousePressed",
                MouseKind::Release => "mouseReleased",
                MouseKind::Drag | MouseKind::Move => "mouseMoved",
            };
            let (name, bit) = match button {
                MouseButton::Left => ("left", 1),
                MouseButton::Right => ("right", 2),
                MouseButton::Middle => ("middle", 4),
                _ => ("none", 0),
            };
            let buttons = match kind {
                MouseKind::Press | MouseKind::Drag => bit,
                _ => 0,
            };
            let mut p = json!({
                "type": ty, "x": px, "y": py, "modifiers": input::cdp_modifiers(*mods),
                "button": if *kind == MouseKind::Move { "none" } else { name },
                "buttons": buttons,
            });
            if matches!(kind, MouseKind::Press | MouseKind::Release) {
                p["clickCount"] = json!((*clicks).max(1));
            }
            Decision::Forward(vec![CdpInput::Mouse(p)])
        }
        BrowserCmd::Wheel { x, y, dx, dy, mods } => {
            let (px, py) = page(*x, *y);
            Decision::Forward(vec![CdpInput::Mouse(json!({
                "type": "mouseWheel", "x": px, "y": py, "deltaX": dx, "deltaY": dy,
                "modifiers": input::cdp_modifiers(*mods),
            }))])
        }
        BrowserCmd::Navigate(_)
        | BrowserCmd::Back
        | BrowserCmd::Forward
        | BrowserCmd::Reload { .. }
        | BrowserCmd::Stop
        | BrowserCmd::Window(_)
        | BrowserCmd::Screenshot => Decision::Unsupported,
    }
}

/// A command from a client for a watch pane.
pub fn command(server: &Arc<Server>, t: &Arc<Target>, cmd: BrowserCmd, key_releases: bool) {
    let (session, here, human, to_page, dpr) = {
        let st = t.st.lock().unwrap();
        let Some(w) = st.watch.as_ref() else { return };
        (
            w.session.clone(),
            w.here,
            w.human,
            w.to_page,
            st.want.map(|g| g.dpr).unwrap_or(1.0),
        )
    };
    match decide(&cmd, here, key_releases, to_page, dpr) {
        Decision::Control(on) => {
            let by = on.then(|| holder(t));
            match agent_browser::set_human_control(server, &session, by) {
                Ok(_) => {
                    {
                        let mut st = t.st.lock().unwrap();
                        if let Some(w) = st.watch.as_mut() {
                            w.human = on;
                            w.here = on;
                        }
                    }
                    notice(
                        t,
                        if on {
                            format!(
                                "you control agent session {session} — the agent gets human_control errors; prefix+t releases"
                            )
                        } else {
                            format!("released agent session {session} — the agent drives again")
                        },
                    );
                }
                Err(e) => notice(t, e.message),
            }
        }
        Decision::Forward(cmds) => {
            let server = server.clone();
            let t = t.clone();
            tokio::spawn(async move {
                if let Err(e) = server.agent_browser.human_input(&session, &cmds).await {
                    notice(&t, format!("input not delivered: {}", e.message));
                }
            });
        }
        Decision::ReadOnly => hint(
            t,
            if human {
                "taken over elsewhere — read-only here; prefix+t takes it over in this pane"
            } else {
                "watching (read-only) — prefix+t takes over"
            },
        ),
        Decision::Unsupported => hint(
            t,
            "not available while watching an agent session (the agent's page; take over and use the page itself)",
        ),
        Decision::Nothing => {}
    }
}

/// The pane is gone (closed, or its target collected): give control back to the agent if this
/// pane held it.
pub fn on_close(server: &Arc<Server>, t: &Arc<Target>) {
    let (session, here) = {
        let mut st = t.st.lock().unwrap();
        let Some(w) = st.watch.as_mut() else { return };
        let here = std::mem::take(&mut w.here);
        (w.session.clone(), here)
    };
    if !here {
        return;
    }
    // Only release what this pane holds (a CLI take-over after ours stays).
    if server
        .agent_browser
        .watch_info(&session)
        .is_some_and(|i| i.human.as_deref() == Some(holder(t).as_str()))
    {
        let _ = agent_browser::set_human_control(server, &session, None);
    }
}

#[cfg(test)]
#[path = "browser_watch_tests.rs"]
mod tests;
