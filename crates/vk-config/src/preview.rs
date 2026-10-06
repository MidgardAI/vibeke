//! Typed `[preview]` keys (06 Part C, 08 §11).
//!
//! `[preview]` stays an external section (`Config::extra["preview"]`) because the preview
//! fabric in `vk-server` reads more of it than this crate needs to know about at runtime; this
//! module gives every Part C key one type and default, validates the section at load time
//! (a bad value is a located warning and falls back to the default for that key only, so one
//! typo never disables previews), and [`crate::Config::preview`] hands out the typed view.

use std::fmt;

use serde::de::{self as de, DeserializeOwned};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::load::Warning;
use crate::units::Dur;

choice_enum!(AutoDiscover { Suggest = "suggest", Promote = "promote", Off = "off" } default Suggest);
choice_enum!(PreviewMode { Pane = "pane", Window = "window", Proxy = "proxy" } default Pane);
choice_enum!(PaneSplit { Right = "right", Down = "down", Tab = "tab", Float = "float" } default Right);
choice_enum!(PaneLocation { Client = "client", Server = "server" } default Client);
choice_enum!(ProfileScope { Machine = "machine", Task = "task" } default Machine);
choice_enum!(ProfileRoute { Loopback = "loopback", Remote = "remote" } default Loopback);
choice_enum!(LocalBrowser { Profile = "profile", Default = "default" } default Profile);
choice_enum!(BrowserExternal { Deny = "deny", Subresources = "subresources", Allow = "allow" } default Subresources);
choice_enum!(ScreenshotFormat { Png = "png", Jpeg = "jpeg", Webp = "webp" } default Png);

/// Accepted `profile_browser` names (an absolute path is accepted too).
pub const PROFILE_BROWSERS: &[&str] = &["auto", "chrome", "chromium", "edge", "brave", "firefox"];

/// `[preview]` (06 Part C).
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    pub auto_discover: AutoDiscover,
    pub mode: PreviewMode,
    pub pane_split: PaneSplit,
    /// Frame cap for browser panes on local clients (remote-rendered panes follow A7).
    pub pane_fps: u32,
    pub pane_location: PaneLocation,
    /// Window browser: one of [`PROFILE_BROWSERS`] or an absolute path.
    pub profile_browser: String,
    pub profile_scope: ProfileScope,
    pub profile_route: ProfileRoute,
    pub allow_remote_egress: bool,
    /// Window browser binary; empty = discover.
    pub browser: String,
    /// Headless Chromium for browser panes; empty = Playwright headless shell.
    pub pane_browser: String,
    pub local_browser: LocalBrowser,
    /// B4 reverse proxy port; 0 = ephemeral.
    pub proxy_port: u16,
    pub tls_origin: bool,
    /// Headless browser binary for agent sessions; empty = discover.
    pub browser_path: String,
    pub browser_idle: Dur,
    pub browser_external: BrowserExternal,
    pub browser_allow_private: Vec<String>,
    pub browser_script: bool,
    /// `WxH` CSS px (see [`Preview::viewport`]).
    pub default_viewport: String,
    pub screenshot_format: ScreenshotFormat,
    pub inline_thumbnails: bool,
}

impl Default for Preview {
    fn default() -> Self {
        Preview {
            auto_discover: AutoDiscover::Suggest,
            mode: PreviewMode::Pane,
            pane_split: PaneSplit::Right,
            pane_fps: 60,
            pane_location: PaneLocation::Client,
            profile_browser: "auto".into(),
            profile_scope: ProfileScope::Machine,
            profile_route: ProfileRoute::Loopback,
            allow_remote_egress: true,
            browser: String::new(),
            pane_browser: String::new(),
            local_browser: LocalBrowser::Profile,
            proxy_port: 47800,
            tls_origin: false,
            browser_path: String::new(),
            browser_idle: Dur::secs(600),
            browser_external: BrowserExternal::Subresources,
            browser_allow_private: Vec::new(),
            browser_script: false,
            default_viewport: "1440x900".into(),
            screenshot_format: ScreenshotFormat::Png,
            inline_thumbnails: true,
        }
    }
}

/// Every key [`Preview::from_value`] knows.
pub const PREVIEW_KEYS: &[&str] = &[
    "auto_discover",
    "mode",
    "pane_split",
    "pane_fps",
    "pane_location",
    "profile_browser",
    "profile_scope",
    "profile_route",
    "allow_remote_egress",
    "browser",
    "pane_browser",
    "local_browser",
    "proxy_port",
    "tls_origin",
    "browser_path",
    "browser_idle",
    "browser_external",
    "browser_allow_private",
    "browser_script",
    "default_viewport",
    "screenshot_format",
    "inline_thumbnails",
];

/// `WxH` with both sides in 100..=10000.
pub fn parse_viewport(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.trim().split_once(['x', 'X'])?;
    let w: u32 = w.trim().parse().ok()?;
    let h: u32 = h.trim().parse().ok()?;
    ((100..=10_000).contains(&w) && (100..=10_000).contains(&h)).then_some((w, h))
}

fn take<T: DeserializeOwned>(
    t: &toml::Table,
    key: &str,
    slot: &mut T,
    warns: &mut Vec<Warning>,
) -> bool {
    let Some(v) = t.get(key) else {
        return false;
    };
    match v.clone().try_into::<T>() {
        Ok(x) => {
            *slot = x;
            true
        }
        Err(e) => {
            let msg = e.to_string();
            let msg = msg.lines().next().unwrap_or("invalid value").trim();
            warns.push(Warning::new(
                format!("preview.{key}"),
                format!("preview.{key}: {msg}; using the default"),
            ));
            false
        }
    }
}

impl Preview {
    /// Typed view of a `[preview]` table plus warnings (unknown keys, bad values). Each bad
    /// key falls back to its default; the rest of the section still applies.
    pub fn from_value(v: Option<&toml::Value>) -> (Preview, Vec<Warning>) {
        let mut p = Preview::default();
        let mut w = Vec::new();
        let Some(v) = v else {
            return (p, w);
        };
        let Some(t) = v.as_table() else {
            w.push(Warning::new(
                "preview",
                "[preview] must be a table; ignored",
            ));
            return (p, w);
        };
        take(t, "auto_discover", &mut p.auto_discover, &mut w);
        take(t, "mode", &mut p.mode, &mut w);
        take(t, "pane_split", &mut p.pane_split, &mut w);
        let mut fps: i64 = p.pane_fps as i64;
        if take(t, "pane_fps", &mut fps, &mut w) {
            if (1..=240).contains(&fps) {
                p.pane_fps = fps as u32;
            } else {
                w.push(Warning::new(
                    "preview.pane_fps",
                    "preview.pane_fps must be 1..=240; using 60",
                ));
            }
        }
        take(t, "pane_location", &mut p.pane_location, &mut w);
        let mut pb = p.profile_browser.clone();
        if take(t, "profile_browser", &mut pb, &mut w) {
            if PROFILE_BROWSERS.contains(&pb.as_str()) || pb.starts_with('/') {
                p.profile_browser = pb;
            } else {
                w.push(Warning::new(
                    "preview.profile_browser",
                    format!(
                        "preview.profile_browser: `{pb}` is not one of {} or an absolute path; using auto",
                        PROFILE_BROWSERS.join(" | ")
                    ),
                ));
            }
        }
        take(t, "profile_scope", &mut p.profile_scope, &mut w);
        take(t, "profile_route", &mut p.profile_route, &mut w);
        take(t, "allow_remote_egress", &mut p.allow_remote_egress, &mut w);
        take(t, "browser", &mut p.browser, &mut w);
        take(t, "pane_browser", &mut p.pane_browser, &mut w);
        take(t, "local_browser", &mut p.local_browser, &mut w);
        let mut port: i64 = p.proxy_port as i64;
        if take(t, "proxy_port", &mut port, &mut w) {
            match u16::try_from(port) {
                Ok(x) => p.proxy_port = x,
                Err(_) => w.push(Warning::new(
                    "preview.proxy_port",
                    "preview.proxy_port must be 0..=65535; using 47800",
                )),
            }
        }
        take(t, "tls_origin", &mut p.tls_origin, &mut w);
        take(t, "browser_path", &mut p.browser_path, &mut w);
        take(t, "browser_idle", &mut p.browser_idle, &mut w);
        take(t, "browser_external", &mut p.browser_external, &mut w);
        take(
            t,
            "browser_allow_private",
            &mut p.browser_allow_private,
            &mut w,
        );
        take(t, "browser_script", &mut p.browser_script, &mut w);
        let mut vp = p.default_viewport.clone();
        if take(t, "default_viewport", &mut vp, &mut w) {
            if parse_viewport(&vp).is_some() {
                p.default_viewport = vp;
            } else {
                w.push(Warning::new(
                    "preview.default_viewport",
                    format!("preview.default_viewport: `{vp}` is not WxH; using 1440x900"),
                ));
            }
        }
        take(t, "screenshot_format", &mut p.screenshot_format, &mut w);
        take(t, "inline_thumbnails", &mut p.inline_thumbnails, &mut w);
        for k in t.keys() {
            if !PREVIEW_KEYS.contains(&k.as_str()) {
                w.push(Warning::new(
                    format!("preview.{k}"),
                    format!("unknown key `preview.{k}`"),
                ));
            }
        }
        (p, w)
    }

    /// `default_viewport` as (width, height) CSS px.
    pub fn viewport(&self) -> (u32, u32) {
        parse_viewport(&self.default_viewport).unwrap_or((1440, 900))
    }
}

impl crate::Config {
    /// The typed `[preview]` section (06 Part C); invalid keys fall back to their defaults
    /// (the warnings come from `Config::parse`).
    pub fn preview(&self) -> Preview {
        Preview::from_value(self.extra.get("preview")).0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn defaults_match_the_spec() {
        let (p, w) = Preview::from_value(None);
        assert!(w.is_empty());
        assert_eq!(p, Preview::default());
        assert_eq!(p.pane_fps, 60);
        assert_eq!(p.viewport(), (1440, 900));
        assert_eq!(p.screenshot_format, ScreenshotFormat::Png);
        assert!(p.inline_thumbnails && p.allow_remote_egress);
    }

    #[test]
    fn typed_values_and_located_fallbacks() {
        let v: toml::Value = toml::from_str(
            r#"
mode = "window"
pane_fps = 30
pane_location = "server"
local_browser = "default"
default_viewport = "1280x720"
screenshot_format = "webp"
inline_thumbnails = false
browser_idle = "2m"
profile_browser = "/Applications/Foo.app/Contents/MacOS/Foo"
"#,
        )
        .unwrap();
        let (p, w) = Preview::from_value(Some(&v));
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(p.mode, PreviewMode::Window);
        assert_eq!(p.pane_fps, 30);
        assert_eq!(p.pane_location, PaneLocation::Server);
        assert_eq!(p.local_browser, LocalBrowser::Default);
        assert_eq!(p.viewport(), (1280, 720));
        assert_eq!(p.screenshot_format, ScreenshotFormat::Webp);
        assert!(!p.inline_thumbnails);
        assert_eq!(p.browser_idle.0, Duration::from_secs(120));

        let bad: toml::Value = toml::from_str(
            r#"
mode = "tabbed"
pane_fps = 0
pane_location = 3
default_viewport = "huge"
screenshot_format = "gif"
profile_browser = "netscape"
proxy_port = 70000
bogus = 1
tls_origin = true
"#,
        )
        .unwrap();
        let (p, w) = Preview::from_value(Some(&bad));
        // Each bad key falls back alone; the good one still applies.
        assert_eq!(p.mode, PreviewMode::Pane);
        assert_eq!(p.pane_fps, 60);
        assert_eq!(p.pane_location, PaneLocation::Client);
        assert_eq!(p.default_viewport, "1440x900");
        assert_eq!(p.screenshot_format, ScreenshotFormat::Png);
        assert_eq!(p.profile_browser, "auto");
        assert_eq!(p.proxy_port, 47800);
        assert!(p.tls_origin);
        let keys: Vec<&str> = w.iter().map(|x| x.key.as_str()).collect();
        for k in [
            "preview.mode",
            "preview.pane_fps",
            "preview.pane_location",
            "preview.default_viewport",
            "preview.screenshot_format",
            "preview.profile_browser",
            "preview.proxy_port",
            "preview.bogus",
        ] {
            assert!(keys.contains(&k), "missing warning for {k}: {keys:?}");
        }
        assert!(
            w.iter()
                .any(|x| x.key == "preview.mode" && x.message.contains("tabbed")),
            "{w:?}"
        );
    }

    #[test]
    fn config_parse_reports_preview_warnings() {
        let (cfg, warns) = crate::Config::parse(
            "[preview]\npane_fps = 999\nmode = \"proxy\"\n",
            std::path::Path::new("config.toml"),
        )
        .unwrap();
        assert_eq!(cfg.preview().mode, PreviewMode::Proxy);
        assert_eq!(cfg.preview().pane_fps, 60);
        let w = warns.iter().find(|w| w.key == "preview.pane_fps").unwrap();
        assert_eq!(w.pos.map(|p| p.line), Some(2), "{w:?}");
    }
}
