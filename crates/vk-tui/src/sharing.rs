//! Hosts in the TUI (16 §15.3–§15.5): the Hosts tab of the Connections view
//! ([`crate::connections`]; `sharing` in the palette, unbound by default) over one machine's
//! gateway, reached through that machine's server with `gateway.call` (the TUI talks to
//! vk-server only; `peer.*` and `share.*` live in the gateway). Four sections, j/k moving
//! through all of them:
//!
//! 1. **Peers** (`peer.list`): the hosts this host can hand work off to, with owner (your host or
//!    a teammate's), expiry and whether this TUI sees that machine online. `x` removes one after a
//!    confirm (`peer.remove`).
//! 2. **Invitations** (`share.list` `invitations`, but for colleagues' shares, which the People
//!    tab ([`crate::people`]) lists): unused links with kind and expiry; `x`
//!    cancels one (`share.revoke`). `n` creates one: `t` **Invite a teammate to send to me**
//!    (`share.create {kind: "handoff", ttl_s}`) or `h` **Pair another of my hosts**
//!    (`peer.invite`). The new link shows with a QR code; `c` copies it through the TUI's
//!    clipboard path (OSC 52, plus the platform tool when not over SSH).
//! 3. **Paste invitation** (`p`, or a bracketed paste into the view): a text field. The link is
//!    parsed here (the app URL with `#/pair?d=…`, the bare `d` value, `vibeke://pair?d=…`) and
//!    says what it is before anything happens ("Handoff invitation from laptop-anna (teammate),
//!    valid 23h", "Pair your host mini"); anything else is refused without a call. Then **Show my
//!    git name and email** (off by default) and **Accept** → `peer.redeem`.
//! 4. **Invited hosts** (`share.list` `devices`, again without shares): handoff and peer devices
//!    that other hosts hold on this host, with kind, owner and expiry; `x` revokes one
//!    (`share.revoke`).
//!
//! Plus **Always ask before importing handoffs** (`a`, `handoff.prefs {always_ask}`).
//!
//! Without a gateway the server answers `remote_unavailable` ("the gateway isn't running: start
//! it with `vibeke gateway on`"); the view shows that message at the top.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use unicode_width::UnicodeWidthStr;
use vk_proto::input::{Key, KeyEvent, KeyKind, NamedKey};
use vk_proto::render::{Color, Style};

use crate::app::{App, Mode, Pending, Popup, RpcErr};
use crate::handoff::{epoch_ms, same_host};
use crate::inbox::fmt_age;
use crate::screen::Grid;

/// `peer.redeem` pairs over the relay: give it longer than the bridge's default 30 s.
pub const REDEEM_TIMEOUT_MS: u64 = 60_000;
/// How long the pairing from a teammate's handoff invitation lasts (the gateway's default).
pub const HANDOFF_TTL_S: u64 = 24 * 3600;

const NOT_A_LINK: &str = "not a Vibeke invitation link";

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn pend(r: Reply) -> Pending {
    Pending::Ux(crate::ux::Reply::Sharing(r))
}

// ---- invitation links -----------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteKind {
    /// A teammate's handoff invitation: redeeming it lets this host send work there.
    Handoff,
    /// Pairs one of the user's own hosts.
    Peer,
    /// A share for the app (view or approve a pane).
    Share,
    /// A plain device pairing link (the app).
    Device,
}

/// What an invitation link says about itself, read locally without contacting anyone.
#[derive(Debug, Clone, PartialEq)]
pub struct Invite {
    pub kind: InviteKind,
    /// The inviting host's name as the link carries it.
    pub host_name: String,
    /// The inviting host's id on the relay (`host`): its identity, unlike the name.
    pub host: String,
    /// Unix seconds: the link must be used before this.
    pub open_by: i64,
    /// Unix seconds: when the pairing it makes ends (handoff invitations).
    pub until: Option<i64>,
    pub label: Option<String>,
}

fn b64(s: &str) -> Option<Vec<u8>> {
    URL_SAFE_NO_PAD.decode(s.trim_end_matches('=')).ok()
}

/// Parse an invitation link (`<app>/#/pair?d=…`, `vibeke://pair?d=…`, or the bare `d` value),
/// the same way the gateway and the app do (`vk_e2e::PairingLink::parse`).
pub fn parse_link(s: &str) -> Result<Invite, String> {
    let s: String = s.split_whitespace().collect();
    if s.is_empty() {
        return Err("paste an invitation link first".into());
    }
    let d = match s.find("d=") {
        Some(i) if s.contains('#') || s.contains('?') => &s[i + 2..],
        _ => s.as_str(),
    };
    let d = d.split('&').next().unwrap_or(d);
    let v: Value = b64(d)
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| NOT_A_LINK.to_string())?;
    if !v.is_object() {
        return Err(NOT_A_LINK.into());
    }
    match v.get("v").and_then(Value::as_u64) {
        Some(1) => {}
        Some(_) => return Err("this invitation needs a newer Vibeke".into()),
        None => return Err(NOT_A_LINK.into()),
    }
    let field = |k: &str| v.get(k).and_then(Value::as_str);
    if ["relay", "host", "pid", "name"]
        .iter()
        .any(|k| field(k).is_none())
        || ["hk", "psk"]
            .iter()
            .any(|k| field(k).and_then(b64).is_none_or(|b| b.len() != 32))
    {
        return Err(NOT_A_LINK.into());
    }
    let open_by = v
        .get("exp")
        .and_then(Value::as_i64)
        .ok_or_else(|| NOT_A_LINK.to_string())?;
    let share = v.get("share").filter(|x| !x.is_null());
    let kind = match share {
        None => InviteKind::Device,
        Some(sh) => match sh.get("kind").and_then(Value::as_str) {
            Some("handoff") => InviteKind::Handoff,
            Some("peer") => InviteKind::Peer,
            Some("share") => InviteKind::Share,
            _ => return Err(NOT_A_LINK.into()),
        },
    };
    Ok(Invite {
        kind,
        host_name: field("name").unwrap_or_default().to_string(),
        host: field("host").unwrap_or_default().to_string(),
        open_by,
        until: share
            .and_then(|x| x.get("until"))
            .and_then(Value::as_i64)
            .filter(|t| *t > 0),
        label: share
            .and_then(|x| x.get("label"))
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

impl Invite {
    fn name(&self) -> String {
        let n = crate::plugins::sanitize(&self.host_name, 64);
        if n.is_empty() { "a host".into() } else { n }
    }

    /// What it is, before anything happens.
    pub fn describe(&self, now_s: i64) -> String {
        let left = |t: i64| fmt_age((t - now_s).max(0) * 1000);
        match self.kind {
            InviteKind::Handoff => format!(
                "Handoff invitation from {} (teammate), valid {}",
                self.name(),
                left(self.until.unwrap_or(self.open_by))
            ),
            InviteKind::Peer => format!("Pair your host {}", self.name()),
            InviteKind::Share => format!("A share of {} for the Vibeke app", self.name()),
            InviteKind::Device => format!("A device pairing link for {}", self.name()),
        }
    }

    /// Why this host can't accept it, if so.
    pub fn refusal(&self, now_s: i64) -> Option<String> {
        match self.kind {
            InviteKind::Share | InviteKind::Device => {
                Some("this invitation is for the Vibeke app: open it there".into())
            }
            _ if self.open_by <= now_s => Some(format!(
                "this invitation expired {} ago: ask for a new one",
                fmt_age((now_s - self.open_by) * 1000)
            )),
            _ => None,
        }
    }
}

/// The link as a QR code (unicode half blocks, quiet zone included).
pub fn render_qr(text: &str) -> Option<String> {
    let code =
        qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::L).ok()?;
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .build(),
    )
}

/// What a failed `gateway.call` says: the server's message (`remote_unavailable` carries "the
/// gateway isn't running: start it with `vibeke gateway on`"), or that the server is too old.
pub(crate) fn bridge_error(e: &RpcErr) -> String {
    if e.is_method_not_found() {
        "this machine's vibeke has no gateway bridge (update it)".into()
    } else {
        e.message.clone()
    }
}

/// The gateway can't be reached at all (no point in trying the other calls).
pub(crate) fn unreachable(e: &RpcErr) -> bool {
    e.kind == "remote_unavailable" || e.is_method_not_found()
}

// ---- state ---------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Peers,
    Invitations,
    Devices,
}

const SECTIONS: [Section; 3] = [Section::Peers, Section::Invitations, Section::Devices];

#[derive(Debug, Clone, PartialEq)]
pub struct PeerRow {
    pub id: String,
    pub name: String,
    pub owner: String,
    /// Unix seconds.
    pub expires_at: Option<i64>,
    pub expired: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct InvitationRow {
    /// The pairing id (what `share.revoke` takes).
    pub id: String,
    pub kind: String,
    pub label: Option<String>,
    /// Unix seconds: the link must be used before this.
    pub link_expires_at: Option<i64>,
    /// Unix seconds: when the device it makes stops working.
    pub device_expires_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DeviceRow {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub scope: String,
    pub owner: Option<String>,
    /// "laptop-anna (Ann <ann@x>)" for a peer device that said who it is.
    pub sender: Option<String>,
    /// Unix seconds.
    pub expires_at: Option<i64>,
}

fn s_of(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

impl PeerRow {
    fn from_value(v: &Value) -> Option<PeerRow> {
        let id = s_of(v, "id")?;
        Some(PeerRow {
            name: s_of(v, "name").unwrap_or_else(|| id.clone()),
            id,
            owner: s_of(v, "owner").unwrap_or_default(),
            expires_at: v["expires_at"].as_i64(),
            expired: v["expired"].as_bool().unwrap_or(false),
        })
    }
}

impl InvitationRow {
    /// None for a colleague's share: those are the People tab's.
    fn from_value(v: &Value) -> Option<InvitationRow> {
        if v.get("kind").and_then(Value::as_str) == Some("share") {
            return None;
        }
        Some(InvitationRow {
            id: s_of(v, "id")?,
            kind: s_of(v, "kind").unwrap_or_else(|| "device".into()),
            label: s_of(v, "label"),
            link_expires_at: v["link_expires_at"].as_i64(),
            device_expires_at: v["device_expires_at"].as_i64(),
        })
    }
}

impl DeviceRow {
    /// None for a colleague's share: those are the People tab's.
    fn from_value(v: &Value) -> Option<DeviceRow> {
        if v.get("kind").and_then(Value::as_str) == Some("share") {
            return None;
        }
        let sender = v.get("sender").filter(|s| !s.is_null()).and_then(|s| {
            let host = s_of(s, "host_name");
            let u = &s["user"];
            let user = match (s_of(u, "name"), s_of(u, "email")) {
                (Some(n), Some(e)) => Some(format!("{n} <{e}>")),
                (n, e) => n.or(e),
            };
            match (host, user) {
                (Some(h), Some(u)) => Some(format!("{h} ({u})")),
                (h, u) => h.or(u),
            }
        });
        Some(DeviceRow {
            id: s_of(v, "id")?,
            kind: s_of(v, "kind").unwrap_or_default(),
            name: s_of(v, "name").unwrap_or_default(),
            scope: s_of(v, "scope").unwrap_or_default(),
            owner: s_of(v, "owner"),
            sender,
            expires_at: v["expires_at"].as_i64(),
        })
    }
}

/// `x` asks first.
#[derive(Debug, Clone, PartialEq)]
pub struct Confirm {
    pub what: Section,
    pub id: String,
    pub label: String,
}

/// A link just created, shown with its QR code.
#[derive(Debug, Clone, PartialEq)]
pub struct Created {
    pub kind: InviteKind,
    pub link: String,
    /// Unix seconds.
    pub open_by: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PasteRow {
    #[default]
    Input,
    ShareUser,
    Accept,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PasteForm {
    pub input: String,
    /// Show my git name and email (`peer.redeem {share_user}`).
    pub share_user: bool,
    pub focus: PasteRow,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct View {
    pub mi: usize,
    pub section: Section,
    pub sel: usize,
    pub peers: Vec<PeerRow>,
    pub invitations: Vec<InvitationRow>,
    pub devices: Vec<DeviceRow>,
    pub always_ask: Option<bool>,
    /// Lists still being read.
    pub loading: u8,
    /// The gateway can't be reached (or the server has no bridge): shown at the top.
    pub error: Option<String>,
    /// The last action's outcome.
    pub notice: Option<String>,
    pub confirm: Option<Confirm>,
    /// `n`: choosing which invitation to create.
    pub choosing: bool,
    pub created: Option<Created>,
    pub paste: Option<PasteForm>,
    /// A change in flight.
    pub busy: Option<String>,
}

impl View {
    fn new(mi: usize) -> View {
        View {
            mi,
            section: Section::Peers,
            sel: 0,
            peers: Vec::new(),
            invitations: Vec::new(),
            devices: Vec::new(),
            always_ask: None,
            loading: 0,
            error: None,
            notice: None,
            confirm: None,
            choosing: false,
            created: None,
            paste: None,
            busy: None,
        }
    }

    fn len(&self, s: Section) -> usize {
        match s {
            Section::Peers => self.peers.len(),
            Section::Invitations => self.invitations.len(),
            Section::Devices => self.devices.len(),
        }
    }

    fn clamp(&mut self) {
        self.sel = self.sel.min(self.len(self.section).saturating_sub(1));
    }

    /// j/k: the next (or previous) row, past a section's end into the next non-empty section.
    fn step(&mut self, down: bool) {
        let n = self.len(self.section);
        let i = SECTIONS
            .iter()
            .position(|s| *s == self.section)
            .unwrap_or(0);
        if down {
            if self.sel + 1 < n {
                self.sel += 1;
            } else if let Some(s) = SECTIONS[i + 1..].iter().find(|s| self.len(**s) > 0) {
                self.section = *s;
                self.sel = 0;
            }
        } else if self.sel > 0 {
            self.sel -= 1;
        } else if let Some(s) = SECTIONS[..i].iter().rev().find(|s| self.len(**s) > 0) {
            self.section = *s;
            self.sel = self.len(*s) - 1;
        }
    }
}

#[derive(Default)]
pub struct State {
    pub view: Option<View>,
}

/// Replies routed back here (through `ux::Reply::Sharing`).
#[derive(Debug, Clone)]
pub enum Reply {
    Peers,
    Shares,
    Prefs,
    SetPrefs,
    Remove { name: String },
    Revoke { what: Section, label: String },
    Create { kind: InviteKind },
    Redeem,
}

/// `gateway.call {method, params}` on machine `mi`, on its own connection (the gateway may take
/// up to the bridge's timeout to answer).
fn gw_call(app: &mut App, mi: usize, method: &str, params: Value, r: Reply) {
    gw_call_pending(app, mi, method, params, pend(r));
}

/// [`gw_call`] with any reply route (the Devices view shares it).
pub(crate) fn gw_call_pending(app: &mut App, mi: usize, method: &str, params: Value, p: Pending) {
    crate::handoff::call_long_pending(
        app,
        mi,
        "gateway.call",
        json!({"method": method, "params": params}),
        p,
    );
}

fn view_mut(app: &mut App) -> Option<&mut View> {
    app.ux.sharing.view.as_mut()
}

// ---- open / refresh ------------------------------------------------------------------------------

/// Open the Hosts tab on machine `mi` ([`crate::connections`] routes the actions).
pub(crate) fn open_on(app: &mut App, mi: usize) {
    if !app.machines.get(mi).is_some_and(|m| m.connected()) {
        app.toast("that machine is offline");
        return;
    }
    app.ux.sharing.view = Some(View::new(mi));
    app.mode = Mode::Popup(Popup::Sharing);
    refresh(app);
}

/// Read the peers, invitations, invited devices and the always-ask preference.
fn refresh(app: &mut App) {
    let Some(v) = view_mut(app) else {
        return;
    };
    v.loading = 3;
    v.error = None;
    let mi = v.mi;
    gw_call(app, mi, "peer.list", json!({}), Reply::Peers);
    gw_call(app, mi, "share.list", json!({}), Reply::Shares);
    app.command_on(mi, "handoff.prefs", json!({}), pend(Reply::Prefs));
}

// ---- actions -------------------------------------------------------------------------------------

/// `x` on the selected row: ask first.
fn ask_remove(v: &mut View) {
    let c = match v.section {
        Section::Peers => v.peers.get(v.sel).map(|p| Confirm {
            what: Section::Peers,
            id: p.id.clone(),
            label: p.name.clone(),
        }),
        Section::Invitations => v.invitations.get(v.sel).map(|i| Confirm {
            what: Section::Invitations,
            id: i.id.clone(),
            label: format!("{} invitation", i.kind),
        }),
        Section::Devices => v.devices.get(v.sel).map(|d| Confirm {
            what: Section::Devices,
            id: d.id.clone(),
            label: if d.name.is_empty() {
                d.id.clone()
            } else {
                d.name.clone()
            },
        }),
    };
    match c {
        Some(c) => v.confirm = Some(c),
        None => v.notice = Some("nothing selected".into()),
    }
}

fn confirmed(app: &mut App, c: Confirm) {
    let Some(v) = view_mut(app) else {
        return;
    };
    let mi = v.mi;
    match c.what {
        Section::Peers => {
            v.busy = Some(format!("removing {}…", c.label));
            gw_call(
                app,
                mi,
                "peer.remove",
                json!({"id": c.id}),
                Reply::Remove { name: c.label },
            );
        }
        what => {
            v.busy = Some(if what == Section::Invitations {
                "cancelling the invitation…".into()
            } else {
                format!("revoking {}…", c.label)
            });
            gw_call(
                app,
                mi,
                "share.revoke",
                json!({"id": c.id}),
                Reply::Revoke {
                    what,
                    label: c.label,
                },
            );
        }
    }
}

fn create(app: &mut App, kind: InviteKind) {
    let Some(v) = view_mut(app) else {
        return;
    };
    v.busy = Some("creating an invitation…".into());
    v.notice = None;
    let mi = v.mi;
    match kind {
        InviteKind::Handoff => gw_call(
            app,
            mi,
            "share.create",
            json!({"kind": "handoff", "ttl_s": HANDOFF_TTL_S}),
            Reply::Create { kind },
        ),
        _ => gw_call(
            app,
            mi,
            "peer.invite",
            json!({}),
            Reply::Create {
                kind: InviteKind::Peer,
            },
        ),
    }
}

fn toggle_always_ask(app: &mut App) {
    let Some(v) = view_mut(app) else {
        return;
    };
    let on = !v.always_ask.unwrap_or(false);
    let mi = v.mi;
    app.command_on(
        mi,
        "handoff.prefs",
        json!({"always_ask": on}),
        pend(Reply::SetPrefs),
    );
}

/// Accept the pasted invitation: parsed and checked here first; nothing is sent for a link
/// that isn't one this host can redeem.
fn redeem(app: &mut App) {
    let Some(v) = view_mut(app) else {
        return;
    };
    let mi = v.mi;
    let Some(f) = v.paste.as_mut() else {
        return;
    };
    let link = f.input.split_whitespace().collect::<String>();
    let inv = match parse_link(&link) {
        Ok(inv) => inv,
        Err(e) => {
            f.error = Some(e);
            return;
        }
    };
    if let Some(why) = inv.refusal(now_ms() / 1000) {
        f.error = Some(why);
        return;
    }
    f.error = None;
    let share_user = f.share_user;
    v.busy = Some(format!("accepting the invitation from {}…", inv.name()));
    crate::handoff::call_long_pending(
        app,
        mi,
        "gateway.call",
        json!({"method": "peer.redeem", "params": {"link": link, "share_user": share_user},
               "timeout_ms": REDEEM_TIMEOUT_MS}),
        pend(Reply::Redeem),
    );
}

fn close(app: &mut App) {
    app.ux.sharing.view = None;
    app.mode = Mode::Normal;
}

/// Leaving the tab for another (a redeem in flight still finishes on the gateway).
pub(crate) fn leave(app: &mut App) {
    app.ux.sharing.view = None;
}

/// A text field has the keys (tab moves within the paste form, not between tabs).
pub(crate) fn typing(app: &App) -> bool {
    app.ux
        .sharing
        .view
        .as_ref()
        .is_some_and(|v| v.paste.is_some())
}

/// The next connected machine after the view's.
fn next_machine(app: &App, mi: usize) -> Option<usize> {
    let n = app.machines.len();
    (1..n)
        .map(|k| (mi + k) % n)
        .find(|&i| app.machines[i].connected())
}

// ---- keys ----------------------------------------------------------------------------------------

pub fn key(app: &mut App, ev: KeyEvent) {
    app.mode = Mode::Popup(Popup::Sharing);
    if ev.kind == KeyKind::Release {
        return;
    }
    let Some(v) = view_mut(app) else {
        app.mode = Mode::Normal;
        return;
    };
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    if v.paste.is_some() {
        paste_key(app, &ev);
        app.dirty = true;
        return;
    }
    if v.created.is_some() {
        if esc || (plain && matches!(ev.key, Key::Char('q'))) {
            v.created = None;
        } else if plain && matches!(ev.key, Key::Char('c')) {
            let link = v
                .created
                .as_ref()
                .map(|c| c.link.clone())
                .unwrap_or_default();
            app.copy_text(&link);
        }
        app.dirty = true;
        return;
    }
    if let Some(c) = v.confirm.take() {
        if plain && matches!(ev.key, Key::Char('y' | 'x') | Key::Named(NamedKey::Enter)) {
            confirmed(app, c);
        }
        app.dirty = true;
        return;
    }
    if v.choosing {
        v.choosing = false;
        match ev.key {
            Key::Char('t') if plain => create(app, InviteKind::Handoff),
            Key::Char('h') if plain => create(app, InviteKind::Peer),
            _ => {}
        }
        app.dirty = true;
        return;
    }
    v.notice = None;
    match ev.key {
        _ if esc => return close(app),
        Key::Char('q') if plain => return close(app),
        Key::Char('j') | Key::Named(NamedKey::Down) => v.step(true),
        Key::Char('k') | Key::Named(NamedKey::Up) => v.step(false),
        Key::Char('x') if plain => ask_remove(v),
        Key::Char('n') if plain => v.choosing = true,
        Key::Char('p') if plain => v.paste = Some(PasteForm::default()),
        Key::Char('a') if plain => toggle_always_ask(app),
        Key::Char('g' | 'r') if plain => refresh(app),
        Key::Char('m') if plain => {
            let mi = v.mi;
            match next_machine(app, mi) {
                Some(next) => open_on(app, next),
                None => app.toast("no other machine is connected"),
            }
        }
        _ => {}
    }
    app.dirty = true;
}

fn paste_key(app: &mut App, ev: &KeyEvent) {
    let Some(v) = view_mut(app) else {
        return;
    };
    let busy = v.busy.is_some();
    let Some(f) = v.paste.as_mut() else {
        return;
    };
    let esc = matches!(ev.key, Key::Named(NamedKey::Escape));
    if esc {
        // A redeem in flight still finishes; its outcome shows in the view.
        v.paste = None;
        return;
    }
    if busy {
        return;
    }
    let plain = !ev.mods.ctrl() && !ev.mods.alt();
    let rows = [PasteRow::Input, PasteRow::ShareUser, PasteRow::Accept];
    let at = rows.iter().position(|r| *r == f.focus).unwrap_or(0);
    let mut submit = false;
    match &ev.key {
        Key::Named(NamedKey::Tab) if ev.mods.shift() => f.focus = rows[(at + 2) % 3],
        Key::Named(NamedKey::Tab) | Key::Named(NamedKey::Down) => f.focus = rows[(at + 1) % 3],
        Key::Named(NamedKey::Up) => f.focus = rows[(at + 2) % 3],
        Key::Named(NamedKey::Enter) => match f.focus {
            PasteRow::ShareUser => f.share_user = !f.share_user,
            _ => submit = true,
        },
        Key::Named(NamedKey::Backspace) if f.focus == PasteRow::Input => {
            f.input.pop();
            f.error = None;
        }
        Key::Char('u') if ev.mods.ctrl() && f.focus == PasteRow::Input => {
            f.input.clear();
            f.error = None;
        }
        Key::Char(c) if plain && f.focus == PasteRow::Input && !c.is_whitespace() => {
            f.input.push(*c);
            f.error = None;
        }
        Key::Named(NamedKey::Space) | Key::Char(' ') if f.focus == PasteRow::ShareUser => {
            f.share_user = !f.share_user
        }
        Key::Named(NamedKey::Space) | Key::Char(' ') if f.focus == PasteRow::Accept => {
            submit = true
        }
        _ => {}
    }
    if submit {
        redeem(app);
    }
}

/// A bracketed paste: into the paste field (opening it when the view shows its sections).
pub fn on_paste(app: &mut App, text: &str) {
    let Some(v) = view_mut(app) else {
        return;
    };
    if v.busy.is_some() || v.created.is_some() || v.confirm.is_some() {
        return;
    }
    v.choosing = false;
    let f = v.paste.get_or_insert_with(PasteForm::default);
    f.focus = PasteRow::Input;
    f.input.extend(text.chars().filter(|c| !c.is_whitespace()));
    f.error = None;
    app.dirty = true;
}

// ---- replies -------------------------------------------------------------------------------------

/// A failed call: an unreachable gateway is the view's banner, anything else its notice.
fn failed(v: &mut View, e: &RpcErr) {
    let msg = bridge_error(e);
    if unreachable(e) {
        v.error = Some(msg);
    } else {
        v.notice = Some(format!("✗ {msg}"));
    }
}

/// `handoff.prefs` answered (`set`: after the toggle).
fn prefs_reply(v: &mut View, res: Result<Value, RpcErr>, set: bool) {
    match res {
        Ok(x) => {
            v.always_ask = x["always_ask"].as_bool();
            if set {
                v.notice = Some(if v.always_ask == Some(true) {
                    "handoffs from your own hosts now wait for you".into()
                } else {
                    "handoffs from your own hosts import without asking when the place is known"
                        .into()
                });
            }
        }
        Err(e) if e.is_method_not_found() => {
            v.notice = Some("this server has no handoff preferences (update it)".into());
        }
        Err(e) => v.notice = Some(format!("✗ {}", e.message)),
    }
}

pub fn on_reply(app: &mut App, mi: usize, r: Reply, res: Result<Value, RpcErr>) {
    app.dirty = true;
    let Some(v) = view_mut(app).filter(|v| v.mi == mi) else {
        return;
    };
    match r {
        Reply::Peers => {
            v.loading = v.loading.saturating_sub(1);
            match res {
                Ok(x) => {
                    v.peers = x["peers"]
                        .as_array()
                        .map(|a| a.iter().filter_map(PeerRow::from_value).collect())
                        .unwrap_or_default();
                }
                Err(e) => failed(v, &e),
            }
        }
        Reply::Shares => {
            v.loading = v.loading.saturating_sub(1);
            match res {
                Ok(x) => {
                    v.invitations = x["invitations"]
                        .as_array()
                        .map(|a| a.iter().filter_map(InvitationRow::from_value).collect())
                        .unwrap_or_default();
                    v.devices = x["devices"]
                        .as_array()
                        .map(|a| a.iter().filter_map(DeviceRow::from_value).collect())
                        .unwrap_or_default();
                }
                Err(e) => failed(v, &e),
            }
        }
        Reply::Prefs => {
            v.loading = v.loading.saturating_sub(1);
            prefs_reply(v, res, false);
        }
        Reply::SetPrefs => prefs_reply(v, res, true),
        Reply::Remove { name } => {
            v.busy = None;
            match res {
                Ok(_) => {
                    v.peers.retain(|p| p.name != name);
                    v.notice = Some(format!(
                        "removed {name}: this host no longer hands off to it"
                    ));
                    gw_call(app, mi, "peer.list", json!({}), Reply::Peers);
                }
                Err(e) => failed(v, &e),
            }
        }
        Reply::Revoke { what, label } => {
            v.busy = None;
            match res {
                Ok(_) => {
                    v.notice = Some(if what == Section::Invitations {
                        format!("cancelled the {label}")
                    } else {
                        format!("revoked {label}")
                    });
                    gw_call(app, mi, "share.list", json!({}), Reply::Shares);
                }
                Err(e) => failed(v, &e),
            }
        }
        Reply::Create { kind } => {
            v.busy = None;
            match res {
                Ok(x) => match x["link"].as_str().filter(|l| !l.is_empty()) {
                    Some(link) => {
                        v.created = Some(Created {
                            kind,
                            link: link.to_string(),
                            open_by: x["open_by"].as_i64(),
                        });
                        gw_call(app, mi, "share.list", json!({}), Reply::Shares);
                    }
                    None => v.notice = Some("✗ the gateway returned no link".into()),
                },
                Err(e) => failed(v, &e),
            }
        }
        Reply::Redeem => {
            v.busy = None;
            match res {
                Ok(x) => {
                    let name = x["peer"]["name"].as_str().unwrap_or("the host").to_string();
                    v.paste = None;
                    v.section = Section::Peers;
                    v.notice = Some(format!("paired with {name}: you can hand off to it now"));
                    gw_call(app, mi, "peer.list", json!({}), Reply::Peers);
                }
                Err(e) => {
                    let msg = bridge_error(&e);
                    if unreachable(&e) {
                        v.error = Some(msg.clone());
                    }
                    match v.paste.as_mut() {
                        Some(f) => f.error = Some(msg),
                        None => v.notice = Some(format!("✗ {msg}")),
                    }
                }
            }
        }
    }
    if let Some(v) = view_mut(app) {
        v.clamp();
    }
}

// ---- drawing -------------------------------------------------------------------------------------

/// "expires in 3h", "expired", or nothing.
fn expiry(t: Option<i64>, now: i64) -> Option<String> {
    let t = epoch_ms(t?);
    Some(if t > now {
        format!("expires in {}", fmt_age(t - now))
    } else {
        "expired".into()
    })
}

fn peer_note(app: &App, p: &PeerRow, now: i64) -> String {
    let mut parts = vec![if p.owner == "self" {
        "your host".to_string()
    } else {
        "teammate".to_string()
    }];
    if p.expired {
        parts.push("expired".into());
    } else if let Some(e) = expiry(p.expires_at, now) {
        parts.push(e);
    }
    if let Some(m) = app.machines.iter().find(|m| same_host(&m.label, &p.name)) {
        parts.push(if m.connected() { "online" } else { "offline" }.into());
    }
    parts.join(" · ")
}

fn invitation_line(i: &InvitationRow, now: i64) -> String {
    let what = match i.kind.as_str() {
        "handoff" => "handoff · a teammate sends to me",
        "peer" => "peer · pairs one of my hosts",
        "share" => "share · the app views a pane",
        _ => "device",
    };
    let mut s = what.to_string();
    if let Some(l) = &i.label {
        s.push_str(&format!(" · “{}”", crate::plugins::sanitize(l, 40)));
    }
    match i.link_expires_at.map(epoch_ms) {
        Some(t) if t > now => s.push_str(&format!(" · open for {}", fmt_age(t - now))),
        Some(_) => s.push_str(" · link expired"),
        None => {}
    }
    if let Some(t) = i.device_expires_at.map(epoch_ms).filter(|t| *t > now) {
        s.push_str(&format!(" · access lasts {}", fmt_age(t - now)));
    }
    s
}

fn device_line(d: &DeviceRow, now: i64) -> String {
    let name = if d.name.is_empty() { &d.id } else { &d.name };
    let mut parts = vec![crate::plugins::sanitize(name, 40), d.kind.clone()];
    match d.owner.as_deref() {
        Some("self") => parts.push("your host".into()),
        Some("teammate") => parts.push("teammate".into()),
        _ if !d.scope.is_empty() => parts.push(d.scope.clone()),
        _ => {}
    }
    if let Some(s) = &d.sender {
        parts.push(format!("from {}", crate::plugins::sanitize(s, 60)));
    }
    if let Some(e) = expiry(d.expires_at, now) {
        parts.push(e);
    }
    parts.join(" · ")
}

/// Draw the view; the cursor position in the paste field.
pub fn draw(app: &App, g: &mut Grid) -> Option<(u16, u16)> {
    let v = app.ux.sharing.view.as_ref()?;
    let t = app.theme;
    let label = app.machines.get(v.mi).map_or("", |m| m.label.as_str());
    let mut a = crate::connections::area(app, g, crate::connections::Tab::Hosts, v.mi);
    let now = now_ms();
    if let Some(e) = &v.error {
        a.line(&format!("⚠ {e}"), t.bold(t.yellow));
        a.line("", t.text());
    }
    if let Some(c) = &v.created {
        draw_created(app, &mut a, c, now);
        return None;
    }
    if let Some(f) = &v.paste {
        return draw_paste(app, &mut a, v, f, now);
    }
    let mark = |s: Section, i: usize| v.section == s && v.sel == i;
    let row_st = |on: bool| if on { t.sel(t.fg) } else { t.text() };
    let head = |s: Section| {
        if v.section == s {
            t.bold(t.accent)
        } else {
            t.bold(t.fg)
        }
    };
    let loading = v.loading > 0;
    a.line(
        &format!("Peers — hosts {label} can hand work off to"),
        head(Section::Peers),
    );
    if v.peers.is_empty() {
        a.line(
            if loading {
                "  loading…"
            } else {
                "  none yet: n → Pair another of my hosts on the other host, or p to paste an invitation"
            },
            t.dim(),
        );
    }
    for (i, p) in v.peers.iter().enumerate() {
        let on = mark(Section::Peers, i);
        a.line(
            &format!(
                "{} {:<24} {}",
                if on { "›" } else { " " },
                crate::plugins::sanitize(&p.name, 24),
                peer_note(app, p, now)
            ),
            row_st(on),
        );
    }
    a.line("", t.text());
    a.line("Invitations — unused links", head(Section::Invitations));
    if v.invitations.is_empty() {
        a.line(if loading { "  loading…" } else { "  none" }, t.dim());
    }
    for (i, inv) in v.invitations.iter().enumerate() {
        let on = mark(Section::Invitations, i);
        a.line(
            &format!(
                "{} {}",
                if on { "›" } else { " " },
                invitation_line(inv, now)
            ),
            row_st(on),
        );
    }
    a.line("", t.text());
    a.line("Paste invitation — p", t.bold(t.fg));
    a.line("", t.text());
    a.line(
        "Invited hosts — teammates' and your own hosts holding access to this host",
        head(Section::Devices),
    );
    if v.devices.is_empty() {
        a.line(if loading { "  loading…" } else { "  none" }, t.dim());
    }
    for (i, d) in v.devices.iter().enumerate() {
        let on = mark(Section::Devices, i);
        a.line(
            &format!("{} {}", if on { "›" } else { " " }, device_line(d, now)),
            row_st(on),
        );
    }
    a.line("", t.text());
    let check = match v.always_ask {
        Some(true) => "[x]",
        Some(false) => "[ ]",
        None => "[?]",
    };
    a.line(
        &format!("{check} Always ask before importing handoffs  (a toggles)"),
        t.text(),
    );
    a.line("", t.text());
    if let Some(c) = &v.confirm {
        let q = match c.what {
            Section::Peers => format!(
                "Remove {}? This host can no longer hand off to it. [y] remove  [other] keep",
                c.label
            ),
            Section::Invitations => format!(
                "Cancel this {}? Its link stops working. [y] cancel it  [other] keep",
                c.label
            ),
            Section::Devices => format!(
                "Revoke {}? It loses its access to this host. [y] revoke  [other] keep",
                c.label
            ),
        };
        a.line(&q, t.bold(t.red));
    } else if v.choosing {
        a.line(
            "New invitation: [t] Invite a teammate to send to me   [h] Pair another of my hosts   [other] back",
            t.bold(t.fg),
        );
    } else if let Some(b) = &v.busy {
        a.line(&format!("⏳ {b}"), t.s(t.yellow));
    } else if let Some(n) = &v.notice {
        a.line(n, t.s(t.yellow));
    }
    let more = if app.machines.len() > 1 {
        " · m machine"
    } else {
        ""
    };
    a.footer(
        &format!(
            "j/k move · x remove/cancel/revoke · n new invitation · p paste · a always ask · g refresh{more} · esc"
        ),
        t.dim(),
    );
    None
}

fn draw_created(app: &App, a: &mut crate::drafts::Area<'_>, c: &Created, now: i64) {
    let t = app.theme;
    let what = match c.kind {
        InviteKind::Handoff => {
            "Invitation for a teammate: once they accept it on one of their hosts, that host can hand work off to this one (nothing else)."
        }
        _ => {
            "Peer invitation: accept it on your other host (Connections → Hosts → p, or `vibeke gateway peer add <link>`) to hand work off to this one."
        }
    };
    a.line(what, t.bold(t.fg));
    if let Some(open) = c.open_by.map(epoch_ms).filter(|x| *x > now) {
        a.line(
            &format!("The link works once, for {}.", fmt_age(open - now)),
            t.dim(),
        );
    }
    a.line("", t.text());
    draw_link_qr(app, a, &c.link);
    a.footer("[c] copy link   [esc] back", t.bold(t.fg));
}

/// The link wrapped to the width, then its QR code (or why there is none; `c` copies the link).
pub(crate) fn draw_link_qr(app: &App, a: &mut crate::drafts::Area<'_>, link: &str) {
    let t = app.theme;
    let w = a.rest().w as usize;
    let chars: Vec<char> = link.chars().collect();
    for chunk in chars.chunks(w.max(20)) {
        a.line(&chunk.iter().collect::<String>(), t.s(t.accent));
    }
    a.line("", t.text());
    let qr_style = Style {
        fg: Color::Rgb(0, 0, 0),
        bg: Color::Rgb(255, 255, 255),
        ..Style::default()
    };
    match render_qr(link) {
        Some(qr) => {
            let lines: Vec<&str> = qr.lines().collect();
            let qw = lines
                .iter()
                .map(|l| UnicodeWidthStr::width(*l))
                .max()
                .unwrap_or(0);
            if lines.len() < a.left() as usize && qw <= w {
                for l in lines {
                    a.line(l, qr_style);
                }
            } else {
                a.line(
                    "(the window is too small for the QR code: c copies the link)",
                    t.dim(),
                );
            }
        }
        None => a.line("(link too long for a QR code: c copies it)", t.dim()),
    }
}

fn draw_paste(
    app: &App,
    a: &mut crate::drafts::Area<'_>,
    v: &View,
    f: &PasteForm,
    now: i64,
) -> Option<(u16, u16)> {
    let t = app.theme;
    a.line(
        "Paste invitation: a teammate's handoff invitation, or a peer invitation from one of your hosts",
        t.bold(t.fg),
    );
    a.line("", t.text());
    let focus_st = t.sel(t.fg);
    let st = |r: PasteRow| if f.focus == r { focus_st } else { t.text() };
    let mark = |r: PasteRow| if f.focus == r { "›" } else { " " };
    let prefix = format!("{} Link  ", mark(PasteRow::Input));
    let r = a.rest();
    let room = (r.w as usize).saturating_sub(UnicodeWidthStr::width(prefix.as_str()) + 1);
    // The tail of a long link stays visible while typing.
    let n = f.input.chars().count();
    let shown: String = f.input.chars().skip(n.saturating_sub(room)).collect();
    let cursor = (
        r.x + (UnicodeWidthStr::width(prefix.as_str()) + UnicodeWidthStr::width(shown.as_str()))
            as u16,
        r.y,
    );
    a.line(&format!("{prefix}{shown}"), st(PasteRow::Input));
    let sec = now / 1000;
    match parse_link(&f.input) {
        _ if f.input.trim().is_empty() => a.line(
            "        paste the link (the app URL, vibeke://pair?d=… or the bare code)",
            t.dim(),
        ),
        Ok(inv) => {
            a.line(&format!("        {}", inv.describe(sec)), t.bold(t.green));
            if let Some(why) = inv.refusal(sec) {
                a.line(&format!("        ✗ {why}"), t.s(t.red));
            }
        }
        Err(e) => a.line(&format!("        ✗ {e}"), t.s(t.red)),
    }
    a.line("", t.text());
    a.line(
        &format!(
            "{} {} Show my git name and email",
            mark(PasteRow::ShareUser),
            if f.share_user { "[x]" } else { "[ ]" }
        ),
        st(PasteRow::ShareUser),
    );
    a.line(
        "      off: the other side only sees this host's name",
        t.dim(),
    );
    a.line("", t.text());
    a.line(
        if f.focus == PasteRow::Accept {
            "  [> Accept <]"
        } else {
            "  [ Accept ]"
        },
        if f.focus == PasteRow::Accept {
            t.bold(t.accent)
        } else {
            t.text()
        },
    );
    if let Some(b) = &v.busy {
        a.line(&format!("⏳ {b}"), t.s(t.yellow));
    } else if let Some(e) = &f.error {
        a.line(&format!("✗ {e}"), t.s(t.red));
    }
    a.footer(
        "type or paste · tab/↓↑ move · space toggles · enter accept · esc back",
        t.dim(),
    );
    (f.focus == PasteRow::Input).then_some(cursor)
}

#[cfg(test)]
#[path = "sharing_tests.rs"]
mod tests;
