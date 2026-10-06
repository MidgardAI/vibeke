//! Rust types for the canonical `config.toml` schema (08 §11). Every field has the §11 default.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

use crate::keys::default_bindings;
use crate::units::{ByteSize, Dur, PortRange};

/// Declares a string-valued enum with a default, `as_str`, and serde support whose error
/// message lists the accepted values.
macro_rules! choice_enum {
    ($(#[$m:meta])* $name:ident { $($var:ident = $s:literal),+ $(,)? } default $def:ident) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        pub enum $name { $($var),+ }

        impl Default for $name {
            fn default() -> Self { $name::$def }
        }

        impl $name {
            /// All accepted spellings.
            pub const VALUES: &'static [&'static str] = &[$($s),+];

            pub fn as_str(&self) -> &'static str {
                match self { $($name::$var => $s),+ }
            }
        }

        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
                s.serialize_str(self.as_str())
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
                struct V;
                impl<'de> de::Visitor<'de> for V {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                        write!(f, "one of {}", $name::VALUES.join(" | "))
                    }
                    fn visit_str<E: de::Error>(self, v: &str) -> Result<$name, E> {
                        match v {
                            $($s => Ok($name::$var),)+
                            _ => Err(E::unknown_variant(v, $name::VALUES)),
                        }
                    }
                }
                d.deserialize_str(V)
            }
        }
    };
}

choice_enum!(ShellMode { Auto = "auto", Login = "login", NonLogin = "non_login" } default Auto);
choice_enum!(GraphemeWidth { Auto = "auto", Unicode = "unicode", Legacy = "legacy" } default Auto);
choice_enum!(AllowDeny { Allow = "allow", Deny = "deny" } default Allow);
choice_enum!(Osc52Read { Deny = "deny", Ask = "ask", Allow = "allow" } default Deny);
choice_enum!(RemoteWrite { AskOnce = "ask_once", Allow = "allow", Deny = "deny" } default AskOnce);
choice_enum!(PasteTranslate { PathsOnly = "paths_only", Embedded = "embedded", Ask = "ask", Off = "off" } default PathsOnly);
choice_enum!(AltgrMode { Auto = "auto", Text = "text", Chord = "chord" } default Auto);
choice_enum!(ShiftEnterLegacy { Cr = "cr", Lf = "lf" } default Cr);
choice_enum!(CopyModeKind { Vi = "vi", Emacs = "emacs" } default Vi);
choice_enum!(CommandType { Shell = "shell", Pane = "pane", Popup = "popup", Float = "float", PluginAction = "plugin_action" } default Shell);
choice_enum!(InteractionOverlay { Off = "off", Unfocused = "unfocused", Always = "always" } default Unfocused);
choice_enum!(ConfirmClose { Running = "running", Always = "always", Never = "never" } default Running);
choice_enum!(SidebarPosition { Left = "left", Right = "right" } default Left);
choice_enum!(ShowStateSource { InferredOnly = "inferred-only", Always = "always", Never = "never" } default InferredOnly);
choice_enum!(TabsPosition { Top = "top", Bottom = "bottom", Hidden = "hidden" } default Top);
choice_enum!(BarPosition { Top = "top", Bottom = "bottom" } default Bottom);
choice_enum!(TileView { Terminal = "terminal", Timeline = "timeline" } default Terminal);
choice_enum!(NotifyChannel { Native = "native", Osc = "osc", Both = "both", None = "none" } default Native);
choice_enum!(ResumeOnRestart { Ask = "ask", Always = "always", Never = "never" } default Ask);
choice_enum!(PolicyEffect { Allow = "allow", Deny = "deny", Ask = "ask" } default Ask);
choice_enum!(Vcs { Auto = "auto", Git = "git", Jj = "jj" } default Auto);
choice_enum!(Checkout { Auto = "auto", Worktree = "worktree", Jj = "jj", Clone = "clone", None = "none" } default Auto);
choice_enum!(OnFinish { Keep = "keep", Archive = "archive", Remove = "remove" } default Keep);
choice_enum!(Transport { Ssh = "ssh" } default Ssh);
choice_enum!(MachineKeybindings { Local = "local", Server = "server" } default Local);
choice_enum!(Bootstrap { Push = "push", RemoteDownload = "remote-download" } default Push);
choice_enum!(InputWhenOffline { Drop = "drop", Ask = "ask" } default Drop);
choice_enum!(PredictiveEcho { Auto = "auto", Always = "always", Never = "never" } default Auto);
choice_enum!(SizePolicy { Latest = "latest", Smallest = "smallest", Pinned = "pinned" } default Latest);
choice_enum!(ThemeMode { Auto = "auto", Light = "light", Dark = "dark" } default Auto);
choice_enum!(UpdateChannel { Stable = "stable", Preview = "preview" } default Stable);

fn t() -> bool {
    true
}
fn s(v: &str) -> String {
    v.to_string()
}

/// The whole configuration.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub onboarding: bool,
    pub config: ConfigMeta,
    pub theme: Theme,
    pub terminal: Terminal,
    pub clipboard: Clipboard,
    pub paste: Paste,
    pub keys: Keys,
    pub ui: Ui,
    pub notifications: Notifications,
    pub agents: Agents,
    pub policy: Policy,
    pub tasks: Tasks,
    pub remote: Remote,
    pub pane: Pane,
    pub render: Render,
    pub compat: Compat,
    pub update: Update,
    /// `[layouts.<name>]`: named declarative layouts (07 §2.14 `LayoutSpec`, parsed by the
    /// server) for `vibeke layout apply <name>` and `workspace create --layout <name>`.
    pub layouts: BTreeMap<String, toml::Value>,
    /// Sections owned by other crates (`collision`, `isolation`, `preview`, `screenshots`, `security`,
    /// `plugins`), preserved verbatim so they are not reported as unknown.
    #[serde(skip_deserializing)]
    pub extra: BTreeMap<String, toml::Value>,
}

/// `[config]` — behaviour of config loading itself (08 §11.2).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfigMeta {
    pub watch: bool,
}
impl Default for ConfigMeta {
    fn default() -> Self {
        ConfigMeta { watch: true }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Theme {
    /// `auto` follows the host terminal's light/dark appearance reported by clients;
    /// `light`/`dark` force it (theme propagation, 03 §10.4).
    pub mode: ThemeMode,
    pub name: String,
    pub auto_switch: bool,
    pub dark_name: String,
    pub light_name: String,
    /// Token overrides (`panel_bg`, `fg`, `accent`, …).
    pub custom: BTreeMap<String, String>,
    /// Default pane palette overrides (`ansi0..15`, `fg`, `bg`, `cursor`).
    pub pane: BTreeMap<String, String>,
}
impl Default for Theme {
    fn default() -> Self {
        Theme {
            mode: ThemeMode::Auto,
            name: s("catppuccin"),
            auto_switch: true,
            dark_name: s("catppuccin"),
            light_name: s("catppuccin-latte"),
            custom: BTreeMap::new(),
            pane: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Terminal {
    pub default_shell: String,
    pub shell_mode: ShellMode,
    /// `follow | home | current | <path>`
    pub new_cwd: String,
    pub term: String,
    pub scrollback_lines: u32,
    pub archive_scrollback: bool,
    pub archive_styles: bool,
    pub archive_max_per_pane: ByteSize,
    pub archive_days: u32,
    pub grapheme_width: GraphemeWidth,
    pub allow_passthrough: bool,
    pub host_overrides: BTreeMap<String, bool>,
    pub env: BTreeMap<String, String>,
}
impl Default for Terminal {
    fn default() -> Self {
        Terminal {
            default_shell: String::new(),
            shell_mode: ShellMode::Auto,
            new_cwd: s("follow"),
            term: s("xterm-256color"),
            scrollback_lines: 10_000,
            archive_scrollback: true,
            archive_styles: false,
            archive_max_per_pane: ByteSize::mib(200),
            archive_days: 30,
            grapheme_width: GraphemeWidth::Auto,
            allow_passthrough: false,
            host_overrides: BTreeMap::new(),
            env: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Clipboard {
    pub osc52_write: AllowDeny,
    pub osc52_read: Osc52Read,
    pub copy_on_select: bool,
    pub primary_selection: bool,
    pub remote_write: RemoteWrite,
    /// Largest unsolicited (OSC 52) clipboard write accepted from a pane; bigger ones are dropped.
    pub remote_write_max_bytes: ByteSize,
    /// Minimum gap between clipboard prompts for the same pane; extra writes are dropped.
    pub remote_write_min_interval: Dur,
}
impl Default for Clipboard {
    fn default() -> Self {
        Clipboard {
            osc52_write: AllowDeny::Allow,
            osc52_read: Osc52Read::Deny,
            copy_on_select: false,
            primary_selection: false,
            remote_write: RemoteWrite::AskOnce,
            remote_write_max_bytes: ByteSize::mib(1),
            remote_write_min_interval: Dur::secs(5),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Paste {
    pub translate: PasteTranslate,
    pub max_auto_bytes: ByteSize,
    pub inbox_retention: Dur,
}
impl Default for Paste {
    fn default() -> Self {
        Paste {
            translate: PasteTranslate::PathsOnly,
            max_auto_bytes: ByteSize::mib(50),
            inbox_retention: Dur::secs(14 * 86400),
        }
    }
}

/// `[keys]`: prefix settings, one `action = "binding"` entry per action, and sub-tables.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Keys {
    pub prefix: String,
    pub prefix_timeout_ms: u32,
    pub prefix_passthrough: bool,
    pub altgr_mode: AltgrMode,
    pub shift_enter_legacy: ShiftEnterLegacy,
    pub copy_mode: CopyMode,
    /// Mode-local keymaps (08 §10.2): navigate-mode, resize-mode and card keys.
    pub navigate: BTreeMap<String, String>,
    pub resize: BTreeMap<String, String>,
    pub card: BTreeMap<String, String>,
    #[serde(rename = "command")]
    pub command: Vec<KeyCommand>,
    /// action → binding, with defaults filled in. An empty string unbinds.
    #[serde(flatten, skip_deserializing)]
    pub bindings: BTreeMap<String, String>,
    #[serde(flatten, skip_serializing)]
    pub(crate) raw_other: BTreeMap<String, toml::Value>,
}
impl Default for Keys {
    fn default() -> Self {
        Keys {
            prefix: s("ctrl+b"),
            prefix_timeout_ms: 1500,
            prefix_passthrough: true,
            altgr_mode: AltgrMode::Auto,
            shift_enter_legacy: ShiftEnterLegacy::Cr,
            copy_mode: CopyMode::default(),
            navigate: BTreeMap::new(),
            resize: BTreeMap::new(),
            card: BTreeMap::new(),
            command: Vec::new(),
            bindings: default_bindings(),
            raw_other: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct CopyMode {
    pub mode: CopyModeKind,
    /// Per-key overrides (key → action).
    #[serde(flatten)]
    pub overrides: BTreeMap<String, String>,
}

/// One `[[keys.command]]` entry.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct KeyCommand {
    pub key: String,
    #[serde(rename = "type")]
    pub kind: CommandType,
    pub command: String,
    pub width: Option<String>,
    pub height: Option<String>,
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    pub title: Option<String>,
    /// e.g. `agent:claude`.
    pub when: Option<String>,
    /// Herdr's label for the binding (`type = "plugin_action"` entries carry one; shown in the
    /// palette). Appended.
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Ui {
    pub interaction_overlay: InteractionOverlay,
    pub max_fps: u32,
    pub background_animation_fps: u32,
    pub animate: bool,
    pub focus_follows_mouse: bool,
    pub focus_follows_mouse_delay_ms: u32,
    pub confirm_close: ConfirmClose,
    pub marked_unread_clears_on_focus: bool,
    pub sidebar: Sidebar,
    pub tabs: Tabs,
    pub status_bar: StatusBar,
    pub sync_input: SyncInput,
    pub interactions: Interactions,
    pub fleet: Fleet,
    /// Outer terminal title sync (OSC 2) with the focused workspace/pane (08 §6.7).
    pub title_sync: bool,
    /// `{workspace}`, `{tab}`, `{pane}`, `{machine}`, `{session}`.
    pub title_format: String,
}
impl Default for Ui {
    fn default() -> Self {
        Ui {
            interaction_overlay: InteractionOverlay::Unfocused,
            max_fps: 120,
            background_animation_fps: 4,
            animate: true,
            focus_follows_mouse: false,
            focus_follows_mouse_delay_ms: 120,
            confirm_close: ConfirmClose::Running,
            marked_unread_clears_on_focus: false,
            sidebar: Sidebar::default(),
            tabs: Tabs::default(),
            status_bar: StatusBar::default(),
            sync_input: SyncInput::default(),
            interactions: Interactions::default(),
            fleet: Fleet::default(),
            title_sync: true,
            title_format: "{workspace} · {pane}".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sidebar {
    pub attention_section: bool,
    pub position: SidebarPosition,
    pub width: u16,
    pub min_width: u16,
    pub max_width: u16,
    pub auto_width: bool,
    pub collapsed: bool,
    pub show_shell_panes: bool,
    pub nest_tasks: bool,
    pub show_state_source: ShowStateSource,
    pub isolation_glyphs: IsolationGlyphs,
    pub token: Vec<SidebarToken>,
}
impl Default for Sidebar {
    fn default() -> Self {
        Sidebar {
            attention_section: true,
            position: SidebarPosition::Left,
            width: 28,
            min_width: 18,
            max_width: 48,
            auto_width: true,
            collapsed: false,
            show_shell_panes: false,
            nest_tasks: true,
            show_state_source: ShowStateSource::InferredOnly,
            isolation_glyphs: IsolationGlyphs::default(),
            token: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct IsolationGlyphs {
    pub host: String,
    pub sandbox: String,
    pub container: String,
    pub vm: String,
}
impl Default for IsolationGlyphs {
    fn default() -> Self {
        IsolationGlyphs {
            host: String::new(),
            sandbox: s("sb"),
            container: s("ct"),
            vm: s("vm"),
        }
    }
}

/// `[[ui.sidebar.token]]` rule: match by harness, state or regex, then rename, recolour or hide.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SidebarToken {
    #[serde(rename = "match")]
    pub matcher: TokenMatch,
    pub label: Option<String>,
    pub color: Option<String>,
    pub hide: bool,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TokenMatch {
    pub harness: Option<String>,
    pub state: Option<String>,
    pub regex: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tabs {
    pub position: TabsPosition,
    pub show_numbers: bool,
}
impl Default for Tabs {
    fn default() -> Self {
        Tabs {
            position: TabsPosition::Top,
            show_numbers: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StatusBar {
    pub enabled: bool,
    pub position: BarPosition,
    pub left: Vec<String>,
    pub center: Vec<String>,
    pub right: Vec<String>,
}
impl Default for StatusBar {
    fn default() -> Self {
        StatusBar {
            enabled: false,
            position: BarPosition::Bottom,
            left: vec![s("machine"), s("task"), s("branch")],
            center: vec![s("attention")],
            right: vec![s("agents_summary"), s("clock")],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncInput {
    pub include_agents: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Interactions {
    pub batch: bool,
}
impl Default for Interactions {
    fn default() -> Self {
        Interactions { batch: true }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Fleet {
    pub tile_view: TileView,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Notifications {
    pub channel: NotifyChannel,
    /// `default | none | <path>`
    pub sound: String,
    pub suppress_when_focused: bool,
    pub coalesce_ms: u32,
    /// `"22:00-07:00"` or empty.
    pub quiet_hours: String,
    /// Explicit channel list (08 §7.1): `toast`, `native`, `osc`, `sound`, `bell`. Empty =
    /// derived from `channel` (native → toast+native, osc fallback when native is unavailable).
    pub channels: Vec<String>,
    pub on: NotifyOn,
}
impl Default for Notifications {
    fn default() -> Self {
        Notifications {
            channel: NotifyChannel::Native,
            sound: s("default"),
            suppress_when_focused: true,
            coalesce_ms: 3000,
            quiet_hours: String::new(),
            channels: Vec::new(),
            on: NotifyOn::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NotifyOn {
    pub needs_approval: bool,
    pub needs_answer: bool,
    pub done: bool,
    pub error: bool,
    pub bell: bool,
    pub osc: bool,
    pub remote_disconnected: bool,
}
impl Default for NotifyOn {
    fn default() -> Self {
        NotifyOn {
            needs_approval: true,
            needs_answer: true,
            done: true,
            error: true,
            bell: false,
            osc: true,
            remote_disconnected: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agents {
    pub auto_detect: bool,
    pub shims: bool,
    pub resume_on_restart: ResumeOnRestart,
    pub name_from_task: bool,
    /// `[agents.harness.<id>]`; defaults for claude, pi and codex are filled in.
    pub harness: BTreeMap<String, Harness>,
}
impl Default for Agents {
    fn default() -> Self {
        Agents {
            auto_detect: true,
            shims: true,
            resume_on_restart: ResumeOnRestart::Ask,
            name_from_task: true,
            harness: default_harnesses(),
        }
    }
}

/// Per-harness options. Which keys apply depends on the harness (04), so all are optional
/// except `enabled`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Harness {
    pub enabled: bool,
    pub integration: Option<String>,
    pub extra_args: Vec<String>,
    pub shim: Option<bool>,
    pub headless_shared: Option<bool>,
}
impl Default for Harness {
    fn default() -> Self {
        Harness {
            enabled: t(),
            integration: None,
            extra_args: Vec::new(),
            shim: None,
            headless_shared: None,
        }
    }
}

pub(crate) fn default_harnesses() -> BTreeMap<String, Harness> {
    let mut m = BTreeMap::new();
    m.insert(
        s("claude"),
        Harness {
            integration: Some(s("hooks")),
            ..Harness::default()
        },
    );
    m.insert(
        s("pi"),
        Harness {
            integration: Some(s("extension")),
            ..Harness::default()
        },
    );
    m.insert(
        s("codex"),
        Harness {
            shim: Some(true),
            headless_shared: Some(false),
            ..Harness::default()
        },
    );
    m
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    pub rule: Vec<PolicyRule>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyRule {
    #[serde(rename = "match")]
    pub matcher: PolicyMatch,
    pub effect: PolicyEffect,
    pub scope: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PolicyMatch {
    pub tool: Option<String>,
    pub command_regex: Option<String>,
    pub path_glob: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tasks {
    /// A path, or `"sibling"` → `../<repo>-<slug>`.
    pub root: String,
    pub vcs: Vcs,
    pub checkout: Checkout,
    pub branch_template: String,
    pub fetch_before_create: bool,
    pub default_agent: String,
    pub port_pool: PortRange,
    pub port_block: u16,
    pub setup_script: String,
    pub copy_files: Vec<String>,
    pub cleanup: Cleanup,
    pub best_of_n: BestOfN,
}
impl Default for Tasks {
    fn default() -> Self {
        Tasks {
            root: s("~/.vibeke/worktrees"),
            vcs: Vcs::Auto,
            checkout: Checkout::Auto,
            branch_template: s("{user}/{slug}"),
            fetch_before_create: true,
            default_agent: s("claude"),
            port_pool: PortRange {
                start: 20000,
                end: 29999,
            },
            port_block: 10,
            setup_script: s(".vibeke/setup.sh"),
            copy_files: vec![s(".env"), s(".env.local")],
            cleanup: Cleanup::default(),
            best_of_n: BestOfN::default(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cleanup {
    pub on_finish: OnFinish,
    pub stale_after: Dur,
    pub auto_gc: bool,
    pub protect_dirty: bool,
}
impl Default for Cleanup {
    fn default() -> Self {
        Cleanup {
            on_finish: OnFinish::Keep,
            stale_after: Dur::secs(14 * 86400),
            auto_gc: false,
            protect_dirty: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BestOfN {
    pub suffix: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Remote {
    pub machine: Vec<Machine>,
    pub input_when_offline: InputWhenOffline,
    pub predictive_echo: PredictiveEcho,
}
impl Default for Remote {
    fn default() -> Self {
        Remote {
            machine: Vec::new(),
            input_when_offline: InputWhenOffline::Drop,
            predictive_echo: PredictiveEcho::Auto,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Machine {
    pub label: String,
    pub address: String,
    pub transport: Transport,
    pub keybindings: MachineKeybindings,
    pub auto_connect: bool,
    pub auto_upgrade: bool,
    pub bootstrap: Bootstrap,
}
impl Default for Machine {
    fn default() -> Self {
        Machine {
            label: String::new(),
            address: String::new(),
            transport: Transport::Ssh,
            keybindings: MachineKeybindings::Local,
            auto_connect: true,
            auto_upgrade: false,
            bootstrap: Bootstrap::Push,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Pane {
    pub size_policy: SizePolicy,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Render {
    pub max_unacked: u32,
}
impl Default for Render {
    fn default() -> Self {
        Render { max_unacked: 2 }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Compat {
    pub herdr_env: bool,
    /// Legacy spelling of `[compat.herdr] enabled`.
    pub herdr_socket: bool,
    /// The Herdr compatibility listener (07 §8.3, M5).
    pub herdr: CompatHerdr,
}
impl Default for Compat {
    fn default() -> Self {
        Compat {
            herdr_env: true,
            herdr_socket: false,
            herdr: CompatHerdr::default(),
        }
    }
}

/// `[compat.herdr]`: the public Herdr-compatible socket. Off by default; plugin brokers work
/// regardless. The socket never lives on Herdr's own path.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct CompatHerdr {
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Update {
    pub channel: UpdateChannel,
    pub version_check: bool,
    pub manifest_check: bool,
}
impl Default for Update {
    fn default() -> Self {
        Update {
            channel: UpdateChannel::Stable,
            version_check: true,
            manifest_check: true,
        }
    }
}
