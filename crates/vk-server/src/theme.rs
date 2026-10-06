//! Theme auto light/dark and propagation (08 §11 `[theme]`, 03 §10.4), server side.
//!
//! Clients report the host terminal's appearance (`client.appearance {dark}` — from OSC 11
//! background luminance or the `CSI ? 996 n` colour-scheme report). The effective appearance is
//! `theme.mode` when forced (`light`/`dark`), else the report of the most recently active client
//! that reported one. Changes emit `theme.changed`, update `SessionModel.appearance`, and new
//! panes get `COLORFGBG` / `VIBEKE_THEME` / `VIBEKE_THEME_NAME` so CLIs pick matching colours.
//! Running panes keep their environment (env can't change under a live process).

use crate::Server;
use crate::api::{R, b, invalid, s};
use crate::core::Tx;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Instant;
use vk_proto::model::Appearance;

#[derive(Default)]
pub struct State {
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    /// client id → (dark, reported at).
    reports: HashMap<String, (bool, Instant, String)>,
    /// Runtime override of `theme.mode` (`theme.set_mode`), not persisted.
    mode_override: Option<String>,
    current: Option<Appearance>,
}

/// `[theme]` keys this module needs.
#[derive(Debug, Clone)]
pub struct ThemeCfg {
    pub mode: String,
    pub name: String,
    pub auto_switch: bool,
    pub dark_name: String,
    pub light_name: String,
}

impl ThemeCfg {
    pub fn load() -> Self {
        let cfg = vk_config::Config::load(vk_config::config_path())
            .map(|(c, _)| c)
            .unwrap_or_default();
        Self::from_config(&cfg)
    }
    pub fn from_config(c: &vk_config::Config) -> Self {
        ThemeCfg {
            mode: c.theme.mode.as_str().to_string(),
            name: c.theme.name.clone(),
            auto_switch: c.theme.auto_switch,
            dark_name: c.theme.dark_name.clone(),
            light_name: c.theme.light_name.clone(),
        }
    }
}

/// Pure resolution: forced mode wins; else the freshest client report; else unknown (dark).
pub fn resolve(
    cfg: &ThemeCfg,
    mode_override: Option<&str>,
    reports: &[(String, bool, Instant)],
) -> Appearance {
    let mode = mode_override.unwrap_or(&cfg.mode).to_string();
    let (known, dark, source) = match mode.as_str() {
        "light" => (true, false, String::new()),
        "dark" => (true, true, String::new()),
        _ => match reports.iter().max_by_key(|(_, _, at)| *at) {
            Some((client, dark, _)) => (true, *dark, client.clone()),
            None => (false, true, String::new()),
        },
    };
    let theme = if cfg.auto_switch && known {
        if dark {
            cfg.dark_name.clone()
        } else {
            cfg.light_name.clone()
        }
    } else {
        cfg.name.clone()
    };
    Appearance {
        known,
        dark,
        mode,
        theme,
        source,
    }
}

/// `COLORFGBG` convention (rxvt): `fg;bg` with 15/0 for dark, 0/15 for light.
pub fn colorfgbg(dark: bool) -> &'static str {
    if dark { "15;0" } else { "0;15" }
}

impl State {
    fn reports(&self) -> Vec<(String, bool, Instant)> {
        self.inner
            .lock()
            .unwrap()
            .reports
            .iter()
            .map(|(k, (d, at, _))| (k.clone(), *d, *at))
            .collect()
    }

    /// The last computed appearance (or a fresh resolution if none yet). Never locks `core`.
    pub fn current(&self) -> Appearance {
        if let Some(a) = self.inner.lock().unwrap().current.clone() {
            return a;
        }
        let cfg = ThemeCfg::load();
        let ov = self.inner.lock().unwrap().mode_override.clone();
        resolve(&cfg, ov.as_deref(), &self.reports())
    }
}

/// Env for a new pane (called from `pane_env`, which must not lock `core`).
pub fn pane_env(server: &Server, env: &mut Vec<(String, String)>) {
    let a = server.theme.current();
    let mut set = |k: &str, v: String| {
        env.retain(|(x, _)| x != k);
        env.push((k.to_string(), v));
    };
    if a.known {
        set("COLORFGBG", colorfgbg(a.dark).to_string());
        set(
            "VIBEKE_THEME",
            if a.dark { "dark" } else { "light" }.to_string(),
        );
    }
    if !a.theme.is_empty() {
        set("VIBEKE_THEME_NAME", a.theme);
    }
}

/// Recompute; on change publish `theme.changed` and update the session model.
pub fn recompute(server: &Server, cfg: &ThemeCfg) -> Appearance {
    let ov = server.theme.inner.lock().unwrap().mode_override.clone();
    let next = resolve(cfg, ov.as_deref(), &server.theme.reports());
    let prev = {
        let mut i = server.theme.inner.lock().unwrap();
        i.current.replace(next.clone())
    };
    let changed = prev.as_ref().is_none_or(|p| {
        (p.known, p.dark, &p.mode, &p.theme) != (next.known, next.dark, &next.mode, &next.theme)
    });
    if changed {
        let mut c = server.core.lock().unwrap();
        c.model.appearance = next.clone();
        let mut tx = Tx::new();
        tx.event(
            "theme.changed",
            json!({}),
            json!({"dark": next.dark, "known": next.known, "mode": next.mode, "theme": next.theme,
                   "source": next.source, "colorfgbg": colorfgbg(next.dark),
                   "previous": prev.map(|p| json!({"dark": p.dark, "theme": p.theme}))}),
        );
        let _ = server.commit(&mut c, tx);
    }
    next
}

pub fn forget_client(server: &Server, client: &str) {
    let had = server
        .theme
        .inner
        .lock()
        .unwrap()
        .reports
        .remove(client)
        .is_some();
    if had {
        recompute(server, &ThemeCfg::load());
    }
}

pub const METHODS: &[(&str, bool)] = &[
    ("client.appearance", true),
    ("theme.get", false),
    ("theme.set_mode", true),
];

pub fn api(server: &Server, ctx: &crate::api::Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        // `{dark: bool, source?: "osc11"|"csi996"|…}` from an attached client.
        "client.appearance" => {
            let Some(dark) = b(p, "dark") else {
                return Some(Err(invalid("missing param `dark` (bool)")));
            };
            server.theme.inner.lock().unwrap().reports.insert(
                ctx.client_id.clone(),
                (
                    dark,
                    Instant::now(),
                    s(p, "source").unwrap_or("client").to_string(),
                ),
            );
            let a = recompute(server, &ThemeCfg::load());
            Ok(json!({"appearance": a, "colorfgbg": colorfgbg(a.dark)}))
        }
        "theme.get" => {
            let a = recompute(server, &ThemeCfg::load());
            let reports: Vec<Value> = server
                .theme
                .inner
                .lock()
                .unwrap()
                .reports
                .iter()
                .map(|(k, (d, at, src))| json!({"client": k, "dark": d, "source": src, "age_ms": at.elapsed().as_millis() as u64}))
                .collect();
            Ok(json!({"appearance": a, "colorfgbg": colorfgbg(a.dark), "reports": reports}))
        }
        // Runtime override (`auto | light | dark`, or null to return to config).
        "theme.set_mode" => {
            let mode = s(p, "mode").map(str::to_string);
            if let Some(m) = &mode
                && !matches!(m.as_str(), "auto" | "light" | "dark")
            {
                return Some(Err(invalid("mode must be auto|light|dark")));
            }
            server.theme.inner.lock().unwrap().mode_override = mode;
            Ok(json!({"appearance": recompute(server, &ThemeCfg::load())}))
        }
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn cfg(mode: &str) -> ThemeCfg {
        ThemeCfg::from_config(&{
            let mut c = vk_config::Config::default();
            c.theme.mode = match mode {
                "light" => vk_config::ThemeMode::Light,
                "dark" => vk_config::ThemeMode::Dark,
                _ => vk_config::ThemeMode::Auto,
            };
            c
        })
    }

    #[test]
    fn resolution_order() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(5);
        let reports = vec![("a".to_string(), true, t0), ("b".to_string(), false, t1)];
        let a = resolve(&cfg("auto"), None, &reports);
        assert!(a.known && !a.dark);
        assert_eq!(a.source, "b");
        assert_eq!(a.theme, "catppuccin-latte");
        let a = resolve(&cfg("dark"), None, &reports);
        assert!(a.dark && a.source.is_empty());
        assert_eq!(a.theme, "catppuccin");
        let a = resolve(&cfg("auto"), Some("light"), &[]);
        assert!(a.known && !a.dark);
        let a = resolve(&cfg("auto"), None, &[]);
        assert!(!a.known);
        assert_eq!(a.theme, "catppuccin");
        assert_eq!(colorfgbg(true), "15;0");
        assert_eq!(colorfgbg(false), "0;15");
    }
}
