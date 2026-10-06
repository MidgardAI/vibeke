//! `vibeke doctor` for assistance (14 §9): diagnose the settings **without generating a paid
//! request** and without contacting any provider.
//!
//! Checks configuration shape, profile and connection resolution, endpoint rules, credential
//! availability (an environment variable is checked for presence only; a key file for its
//! mode and owner; a keychain item is never queried because that can prompt), pricing for
//! cost caps, consent grants against the current connections, the background opt-ins and the
//! execution machine. Messages name connections, profiles and variable names the user chose;
//! they never contain a configured value or a secret.

use crate::config::{AssistConfig, Credential, resolve_credential_with, validate_endpoint};
use crate::consent::Grant;
use crate::ops;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Pass,
    Info,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    pub level: Level,
    pub message: String,
    pub hint: Option<String>,
}

pub struct Input<'a> {
    /// The parsed `[assistant]` table, or the (content-free) load error.
    pub config: Result<AssistConfig, String>,
    pub grants: &'a [Grant],
    /// Name of this machine (requests run here; `localhost` endpoints mean this machine).
    pub machine: &'a str,
    /// Environment lookup (injected for tests).
    pub env: &'a dyn Fn(&str) -> Option<String>,
}

fn f(level: Level, message: impl Into<String>) -> Finding {
    Finding {
        level,
        message: message.into(),
        hint: None,
    }
}

fn fh(level: Level, message: impl Into<String>, hint: impl Into<String>) -> Finding {
    Finding {
        level,
        message: message.into(),
        hint: Some(hint.into()),
    }
}

pub fn diagnose(i: &Input<'_>) -> Vec<Finding> {
    let mut out = vec![];
    let cfg = match &i.config {
        Ok(c) => c,
        Err(e) => {
            out.push(fh(
                Level::Fail,
                format!("the [assistant] configuration cannot be loaded: {e}"),
                "fix config.toml; assistance stays off until it parses",
            ));
            return out;
        }
    };
    out.push(f(
        if cfg.enabled {
            Level::Pass
        } else {
            Level::Info
        },
        if cfg.enabled {
            "assistance is enabled"
        } else {
            "assistance is off (the default): no provider is contacted"
        },
    ));
    out.push(f(
        Level::Info,
        format!(
            "requests run on this machine ({}); credentials and localhost endpoints refer to it",
            i.machine
        ),
    ));
    if cfg.connections.is_empty() && cfg.profiles.is_empty() {
        out.push(fh(
            Level::Info,
            "no connections or profiles configured",
            "add [assistant.connections.<id>] and [assistant.profiles.<id>] (see `vibeke --default-config`)",
        ));
        background(cfg, &mut out);
        return out;
    }

    // Connections: endpoint rules and credentials.
    for (id, c) in &cfg.connections {
        let endpoint = c
            .endpoint
            .clone()
            .unwrap_or_else(|| c.adapter.default_endpoint().to_string());
        match validate_endpoint(&endpoint) {
            Ok(_) => out.push(f(
                Level::Pass,
                format!(
                    "connection `{id}` ({}): endpoint is allowed",
                    c.adapter.as_str()
                ),
            )),
            Err(e) => out.push(fh(
                Level::Fail,
                format!("connection `{id}`: {}", e.message),
                "use https://, or http:// only for a loopback endpoint",
            )),
        }
        out.extend(credential(cfg, id, c, i));
    }

    // Profiles: resolution, limits and pricing.
    for (id, p) in &cfg.profiles {
        match cfg.resolve(Some(id)) {
            Ok(r) => {
                out.push(f(
                    Level::Pass,
                    format!(
                        "profile `{id}`: connection `{}`, model `{}`",
                        r.connection_id, r.profile.model
                    ),
                ));
                if r.prices().is_none() {
                    if cfg.daily_cost_limit_usd.is_some() {
                        out.push(fh(
                            Level::Fail,
                            format!("profile `{id}`: a daily cost limit is set but the model has no known pricing, so requests are refused"),
                            "set input_usd_per_mtok and output_usd_per_mtok on the profile, or remove daily_cost_limit_usd",
                        ));
                    } else {
                        out.push(f(
                            Level::Info,
                            format!("profile `{id}`: pricing is unknown, so cost is reported as unknown (not zero)"),
                        ));
                    }
                }
                for (name, support) in &p.capabilities {
                    if crate::capability::Feature::parse(name).is_none() {
                        out.push(f(
                            Level::Warn,
                            format!("profile `{id}`: `{name}` is not a capability name (text, streaming, json_schema, tools, images)"),
                        ));
                    } else {
                        out.push(f(
                            Level::Info,
                            format!("profile `{id}`: {name} declared {}", support.as_str()),
                        ));
                    }
                }
            }
            Err(e) => out.push(fh(
                Level::Fail,
                format!("profile `{id}`: {}", e.message),
                "fix the profile; it fails visibly rather than falling back to another one",
            )),
        }
    }
    if cfg.enabled {
        match cfg.resolve(None) {
            Ok(_) => out.push(f(
                Level::Pass,
                format!("default profile `{}` resolves", cfg.default_profile),
            )),
            Err(e) => out.push(fh(
                Level::Fail,
                format!("default profile: {}", e.message),
                "set [assistant] default_profile to an existing profile",
            )),
        }
    }
    for name in [
        crate::config::BACKGROUND_PROFILE,
        crate::config::REVIEW_PROFILE,
    ] {
        if !cfg.profiles.contains_key(name) {
            continue;
        }
        if let Err(e) = cfg.resolve(Some(name)) {
            out.push(fh(
                Level::Fail,
                format!("the `{name}` profile exists but does not resolve: {}", e.message),
                "a misconfigured feature profile fails visibly; it is never replaced by the default",
            ));
        }
    }

    // Limits and auto_send.
    if cfg.requests_per_minute == 0 || cfg.daily_request_limit == 0 || cfg.daily_token_limit == 0 {
        out.push(f(
            Level::Warn,
            "a request or token limit is zero: every request will be refused",
        ));
    }
    if cfg.max_concurrent_requests == 0 {
        out.push(f(
            Level::Warn,
            "max_concurrent_requests is 0; one request at a time is used",
        ));
    }
    for op in &cfg.auto_send {
        if ops::Operation::parse(op).is_none() {
            out.push(f(
                Level::Warn,
                format!("auto_send names an unknown operation `{op}`"),
            ));
        }
    }

    // Consent grants against the current connections.
    for g in i.grants {
        let status = match cfg
            .connections
            .get(&g.connection)
            .and_then(|_| profile_for(cfg, &g.connection))
        {
            None => Some(format!(
                "consent for {} names connection `{}`, which is no longer configured",
                g.workspace, g.connection
            )),
            Some(r) if r.fingerprint != g.fingerprint => Some(format!(
                "consent for {} on connection `{}` was invalidated: its adapter or endpoint changed",
                g.workspace, g.connection
            )),
            Some(_) => None,
        };
        match status {
            Some(m) => out.push(fh(
                Level::Warn,
                m,
                "grant consent again with `vibeke assist consent`",
            )),
            None => out.push(f(
                Level::Pass,
                format!(
                    "consent for {} on connection `{}` is current",
                    g.workspace, g.connection
                ),
            )),
        }
    }
    if cfg.enabled && i.grants.is_empty() {
        out.push(fh(
            Level::Info,
            "no workspace has granted consent yet, so nothing can be sent",
            "run `vibeke assist consent` in a workspace",
        ));
    }
    background(cfg, &mut out);
    if cfg.result_cache {
        out.push(f(
            Level::Info,
            "result caching is on: identical context under the same grants reuses a stored result without a provider call",
        ));
    }
    out
}

fn profile_for(cfg: &AssistConfig, connection: &str) -> Option<crate::config::Resolved> {
    cfg.profiles
        .iter()
        .find(|(_, p)| p.connection == connection)
        .and_then(|(id, _)| cfg.resolve(Some(id)).ok())
}

fn background(cfg: &AssistConfig, out: &mut Vec<Finding>) {
    if cfg.background_enabled && !cfg.enabled {
        out.push(fh(
            Level::Warn,
            "background_enabled is set but assistance is off: nothing runs in the background",
            "set enabled = true as well, or remove background_enabled",
        ));
    }
    if (cfg.background_summaries || cfg.stall_notices) && !cfg.background_enabled {
        out.push(fh(
            Level::Warn,
            "background_summaries or stall_notices is set but background_enabled is off: they have no effect",
            "background features need their own opt-in on top: set background_enabled = true",
        ));
    }
    if cfg.background_active() {
        out.push(f(
            Level::Info,
            format!(
                "background features are on (summaries: {}, stall notices: {}); they need auto_send in both the config and the workspace consent",
                cfg.background_summaries, cfg.stall_notices
            ),
        ));
        for (name, feature) in [
            (ops::Operation::BackgroundSummary, cfg.background_summaries),
            (ops::Operation::StallNotice, cfg.stall_notices),
        ] {
            if feature && !cfg.auto_send_allows(name.as_str()) {
                out.push(fh(
                    Level::Warn,
                    format!(
                        "{} is on but not in [assistant] auto_send: it can never send",
                        name.as_str()
                    ),
                    "background requests have no one to confirm a preview",
                ));
            }
        }
        if let Err(e) = cfg.resolve_for(crate::config::Purpose::Background, None) {
            out.push(fh(
                Level::Fail,
                format!("no usable background profile: {}", e.message),
                "add [assistant.profiles.background] with a small model",
            ));
        }
    }
}

fn credential(
    cfg: &AssistConfig,
    id: &str,
    c: &crate::config::Connection,
    i: &Input<'_>,
) -> Vec<Finding> {
    let Some(cred) = &c.credential else {
        return vec![if c.adapter.needs_credential() {
            fh(
                Level::Fail,
                format!(
                    "connection `{id}`: no credential reference ({} adapter)",
                    c.adapter.as_str()
                ),
                "add credential = { env = \"NAME\" } (or a 0600 { file = ... })",
            )
        } else {
            f(
                Level::Pass,
                format!("connection `{id}`: no credential needed"),
            )
        }];
    };
    let Credential {
        env,
        file,
        keychain,
    } = cred;
    let n = [env.is_some(), file.is_some(), keychain.is_some()]
        .iter()
        .filter(|b| **b)
        .count();
    if n != 1 {
        return vec![fh(
            Level::Fail,
            format!(
                "connection `{id}`: the credential must name exactly one of env, file or keychain"
            ),
            "credential references are exclusive: no fallback between sources",
        )];
    }
    if let Some(name) = env {
        return vec![match (i.env)(name).map(|v| !v.trim().is_empty()) {
            Some(true) => f(
                Level::Pass,
                format!(
                    "connection `{id}`: credential variable {name} is set (value not read into this report)"
                ),
            ),
            _ => fh(
                Level::Fail,
                format!("connection `{id}`: credential variable {name} is not set on this machine"),
                "export it where the server runs; there is no fallback to ambient provider credentials",
            ),
        }];
    }
    if keychain.is_some() {
        return vec![
            match crate::keychain::parse_backend(&cfg.keychain_backend) {
                Some(crate::keychain::Backend::Off) => fh(
                    Level::Fail,
                    format!("connection `{id}`: keychain credentials are off"),
                    "remove the [assistant] keychain_backend = \"off\" override to use [security] keychain, or use an env or file credential",
                ),
                Some(_) => f(
                    Level::Info,
                    format!(
                        "connection `{id}`: keychain credential not queried here (that can prompt); the first request resolves it"
                    ),
                ),
                None => fh(
                    Level::Fail,
                    format!(
                        "connection `{id}`: the keychain backend must be os or file:<path> (or off/fake)"
                    ),
                    "fix [security] keychain or [assistant] keychain_backend",
                ),
            },
        ];
    }
    // A key file: mode, owner and location are checked by the same code the request path uses.
    match resolve_credential_with(c, &cfg.keychain_backend) {
        Ok(_) => vec![f(
            Level::Pass,
            format!("connection `{id}`: credential file is readable and private"),
        )],
        Err(e) => vec![fh(
            Level::Fail,
            format!("connection `{id}`: {}", e.message),
            "create a dedicated key file owned by you with mode 0600",
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run(cfg: serde_json::Value, grants: &[Grant], env: &[(&str, &str)]) -> Vec<Finding> {
        let env: Vec<(String, String)> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let lookup = move |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        diagnose(&Input {
            config: AssistConfig::from_json(cfg),
            grants,
            machine: "laptop",
            env: &lookup,
        })
    }

    fn has(fs: &[Finding], level: Level, needle: &str) -> bool {
        fs.iter()
            .any(|x| x.level == level && x.message.contains(needle))
    }

    fn good() -> serde_json::Value {
        json!({
            "enabled": true,
            "connections": {"primary": {"adapter": "anthropic", "credential": {"env": "KEY_VAR"}}},
            "profiles": {"interactive": {"connection": "primary", "model": "claude-haiku-4-5-20251001"}},
        })
    }

    #[test]
    fn a_default_install_is_off_and_quiet() {
        let fs = run(json!({}), &[], &[]);
        assert!(has(&fs, Level::Info, "assistance is off"));
        assert!(has(&fs, Level::Info, "no connections or profiles"));
        assert!(fs.iter().all(|x| x.level != Level::Fail));
        assert!(has(&fs, Level::Info, "laptop"));
    }

    #[test]
    fn a_good_setup_passes_and_a_missing_variable_fails_without_fallback() {
        let fs = run(good(), &[], &[("KEY_VAR", "sk-secret-value")]);
        assert!(has(&fs, Level::Pass, "credential variable KEY_VAR is set"));
        assert!(has(
            &fs,
            Level::Pass,
            "default profile `interactive` resolves"
        ));
        assert!(
            fs.iter().all(|x| !x.message.contains("sk-secret-value")),
            "the report never carries a secret"
        );
        let fs = run(good(), &[], &[]);
        assert!(has(&fs, Level::Fail, "KEY_VAR is not set"));
        let fs = run(good(), &[], &[("KEY_VAR", "  ")]);
        assert!(has(&fs, Level::Fail, "KEY_VAR is not set"));
    }

    #[test]
    fn config_and_endpoint_problems_are_located_without_values() {
        let fs = run(json!({"enabeld": true}), &[], &[]);
        assert!(has(&fs, Level::Fail, "unknown key `enabeld`"));
        let mut c = good();
        c["connections"]["primary"]["endpoint"] = json!("http://example.com");
        let fs = run(c, &[], &[("KEY_VAR", "x")]);
        assert!(has(
            &fs,
            Level::Fail,
            "plain HTTP is only allowed for loopback"
        ));
        let mut c = good();
        c["profiles"]["interactive"]["connection"] = json!("nope");
        let fs = run(c, &[], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Fail, "unknown connection `nope`"));
        assert!(has(&fs, Level::Fail, "default profile"));
    }

    #[test]
    fn cost_caps_need_pricing_and_unknown_pricing_is_explained() {
        let mut c = good();
        c["profiles"]["interactive"]["model"] = json!("some-local-model");
        let fs = run(c.clone(), &[], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Info, "pricing is unknown"));
        c["daily_cost_limit_usd"] = json!(1.0);
        let fs = run(c, &[], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Fail, "no known pricing"));
    }

    #[test]
    fn keychain_references_depend_on_the_backend_and_are_never_queried() {
        let mut c = good();
        c["connections"]["primary"]["credential"] = json!({"keychain": "vibeke/assistant/primary"});
        // The default inherits [security] keychain: referenced, never queried here.
        let fs = run(c.clone(), &[], &[]);
        assert!(has(&fs, Level::Info, "not queried here"));
        // The deprecated `off` override still turns keychain references off.
        c["keychain_backend"] = json!("off");
        let fs = run(c.clone(), &[], &[]);
        assert!(has(&fs, Level::Fail, "keychain credentials are off"));
        c["keychain_backend"] = json!("os");
        let fs = run(c, &[], &[]);
        assert!(has(&fs, Level::Info, "not queried here"));
        let mut both = good();
        both["connections"]["primary"]["credential"] = json!({"env": "A", "keychain": "b"});
        assert!(has(&run(both, &[], &[]), Level::Fail, "exactly one of"));
    }

    fn grant(conn: &str, fp: &str) -> Grant {
        Grant {
            workspace: "/w".into(),
            connection: conn.into(),
            fingerprint: fp.into(),
            adapter: "anthropic".into(),
            endpoint_host: "h".into(),
            operations: vec![],
            classes: vec![],
            auto_send: vec![],
            granted_at_ms: 0,
            granted_by: "u".into(),
        }
    }

    #[test]
    fn consent_is_checked_against_current_connections() {
        let cfg = AssistConfig::from_json(good()).unwrap();
        let fp = cfg.resolve(None).unwrap().fingerprint;
        let fs = run(good(), &[grant("primary", &fp)], &[("KEY_VAR", "x")]);
        assert!(has(
            &fs,
            Level::Pass,
            "consent for /w on connection `primary` is current"
        ));
        let fs = run(good(), &[grant("primary", "stale")], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Warn, "invalidated"));
        let fs = run(good(), &[grant("gone", &fp)], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Warn, "no longer configured"));
        let fs = run(good(), &[], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Info, "no workspace has granted consent"));
    }

    #[test]
    fn background_settings_that_cannot_work_are_flagged() {
        let mut c = good();
        c["background_summaries"] = json!(true);
        let fs = run(c.clone(), &[], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Warn, "have no effect"));
        c["background_enabled"] = json!(true);
        let fs = run(c.clone(), &[], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Warn, "not in [assistant] auto_send"));
        c["auto_send"] = json!(["background_summary"]);
        let fs = run(c, &[], &[("KEY_VAR", "x")]);
        assert!(!has(&fs, Level::Warn, "not in [assistant] auto_send"));
        let mut c = good();
        c["profiles"]["background"] = json!({"connection": "ghost", "model": "m"});
        let fs = run(c, &[], &[("KEY_VAR", "x")]);
        assert!(has(
            &fs,
            Level::Fail,
            "`background` profile exists but does not resolve"
        ));
    }

    #[test]
    fn declared_capabilities_and_zero_limits_are_reported() {
        let mut c = good();
        c["profiles"]["interactive"]["capabilities"] =
            json!({"streaming": "supported", "teleport": "supported"});
        c["requests_per_minute"] = json!(0);
        c["auto_send"] = json!(["not_an_operation"]);
        let fs = run(c, &[], &[("KEY_VAR", "x")]);
        assert!(has(&fs, Level::Info, "streaming declared supported"));
        assert!(has(&fs, Level::Warn, "`teleport` is not a capability name"));
        assert!(has(&fs, Level::Warn, "limit is zero"));
        assert!(has(&fs, Level::Warn, "unknown operation"));
    }
}
