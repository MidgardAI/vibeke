//! Real Herdr plugins, unchanged, against Vibeke's compatibility layer (spec 18 §12).
//!
//! One `#[ignore]` test per locked plugin (`tests/compat/herdr/0.9.3/plugins/top20.lock.toml`).
//! They run third-party code, so only the `herdr-plugins` CI job runs them, one plugin per
//! disposable execution boundary, with `--run-ignored only` and `VIBEKE_HERDR_PLUGINS=1`
//! (18 §5). Without that variable each test fails with a clear message and runs no plugin code.
//! The run itself lives in `support::real_plugins`.
//!
//! The normal tests here check that the test list matches the lock, that every scenario is
//! valid, and that each scenario covers what its manifest declares (18 §8.1).

use crate::support::real_plugins::{
    self, ENV_ALLOW, TOOLCHAIN_PATH_VAR, combine_reason, describe_failed_call,
    fake_agent_report_args, final_invocation, has_url_credentials, herdr_install_args,
    herdr_opened_pane, herdr_pane_open_args, herdr_path_component, herdr_result,
    invocation_context, invocation_details, link_position, link_print_parts, log_output,
    names_path_under, plugin_env, popup_open_entrypoint, rate_limited, secret_like, sh_quote,
    shot_panes, sink_request_matches, user_bin_dirs, vibeke_config, vibeke_key, without_path_dirs,
};
use serde_json::json;
use std::collections::BTreeMap;
use std::path::Path;
use vk_conformance::coverage;
use vk_conformance::mask;
use vk_conformance::scenario::Scenario;

/// `plugin_<rank>_<stem>` tests. A lock change makes `the_test_list_matches_the_lock` fail
/// until this list is updated.
macro_rules! locked_plugins {
    ($($name:ident = $rank:literal, $stem:literal;)*) => {
        const LOCKED: &[(u32, &str)] = &[$(($rank, $stem)),*];
        $(
            #[test]
            #[ignore = "runs third-party plugin code: CI only, inside an execution boundary (spec 18 §5)"]
            #[allow(non_snake_case)]
            fn $name() {
                real_plugins::run_locked($rank, $stem);
            }
        )*
    };
}

locked_plugins! {
    plugin_01_zenbu_labs__terminal_browser = 1, "zenbu-labs__terminal-browser";
    plugin_02_zenbu_labs__terminal_code = 2, "zenbu-labs__terminal-code";
    plugin_03_openclaw__crabbox = 3, "openclaw__crabbox";
    plugin_04_furkankly__zoetrope = 4, "furkankly__zoetrope";
    plugin_05_persiyanov__herdr_reviewr = 5, "persiyanov__herdr-reviewr";
    plugin_06_devswha__herdr_web_ui = 6, "devswha__herdr-web-ui";
    plugin_07_plannotator__herdr_annotate__annotate = 7, "plannotator__herdr-annotate__annotate";
    plugin_08_plannotator__herdr_annotate__annotate_links = 8, "plannotator__herdr-annotate__annotate-links";
    plugin_09_smarzban__herdr_file_viewer = 9, "smarzban__herdr-file-viewer";
    plugin_10_eliasstravik__herdr_projects = 10, "eliasstravik__herdr-projects";
    plugin_11_ZingerLittleBee__Heeler = 11, "ZingerLittleBee__Heeler";
    plugin_12_eugenioenko__ttt = 12, "eugenioenko__ttt";
    plugin_13_alexarthurs__herdr_sidebar = 13, "alexarthurs__herdr-sidebar";
    plugin_14_dcolinmorgan__herdr_remote = 14, "dcolinmorgan__herdr-remote";
    plugin_15_cloudmanic__herdr_plus = 15, "cloudmanic__herdr-plus";
    plugin_16_osolmaz__pi_workflows = 16, "osolmaz__pi-workflows";
    plugin_17_0cv__herdr_mobile_relay = 17, "0cv__herdr-mobile-relay";
    plugin_18_powerfooI__roamgate = 18, "powerfooI__roamgate";
    plugin_19_ChmaraX__herdr_nvim = 19, "ChmaraX__herdr-nvim";
    plugin_20_nicosuave__memex = 20, "nicosuave__memex";
}

/// The test name for a lock entry: non-identifier characters become `_`.
fn test_name(rank: u32, stem: &str) -> String {
    let s: String = stem
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    format!("plugin_{rank:02}_{s}")
}

#[test]
fn the_test_list_matches_the_lock() {
    let lock = real_plugins::load_lock();
    lock.validate().expect("valid lock");
    let want: Vec<(u32, String)> = lock
        .plugins
        .iter()
        .map(|p| (p.rank, p.stem(&lock)))
        .collect();
    let have: Vec<(u32, String)> = LOCKED.iter().map(|(r, s)| (*r, s.to_string())).collect();
    assert_eq!(
        have,
        want,
        "update `locked_plugins!` to these entries:\n{}",
        want.iter()
            .map(|(r, s)| format!("    {} = {r}, \"{s}\";", test_name(*r, s)))
            .collect::<Vec<_>>()
            .join("\n")
    );
    // The macro's names follow the same rule; the source spells them out.
    let src = include_str!("compat_herdr_real_plugins.rs");
    for (r, s) in &want {
        assert!(
            src.contains(&format!("{} = {r}, \"{s}\";", test_name(*r, s))),
            "test name for {s}"
        );
    }
}

/// Every scenario that exists is valid and covers its manifest at the locked commit
/// (18 §8.1). Passes while no scenario exists yet.
#[test]
fn scenarios_are_valid_and_cover_their_manifests() {
    let lock = real_plugins::load_lock();
    let mut failures = Vec::new();
    let mut checked = 0;
    for lp in &lock.plugins {
        let stem = lp.stem(&lock);
        let path = real_plugins::scenario_path(&stem);
        if !path.exists() {
            continue;
        }
        let sc = match Scenario::load(&path) {
            Ok(sc) => sc,
            Err(e) => {
                failures.push(format!("{stem}: {e:#}"));
                continue;
            }
        };
        if sc.plugin_id != lp.plugin_id || sc.repository != lp.repository {
            failures.push(format!(
                "{stem}: scenario names {} in {}, the lock {} in {}",
                sc.plugin_id, sc.repository, lp.plugin_id, lp.repository
            ));
        }
        for step in &sc.steps {
            for c in &step.compare {
                if let Some(size) = c.size
                    && let Err(e) = mask::validate(&c.masks, size.cols, size.rows)
                {
                    failures.push(format!("{stem} / {} / {}: {e:#}", step.id, c.pane));
                }
            }
        }
        let Some(m) = real_plugins::checked_in_manifest(&lock, lp) else {
            eprintln!(
                "note: {stem}: no checked-in manifest matches the locked sha256; the CI plugin job checks coverage against the mirror"
            );
            continue;
        };
        for os in ["linux", "macos"] {
            if !lp.supports(os) {
                continue;
            }
            let r = coverage::check(&sc, &m, os);
            if !r.ok() {
                failures.push(format!(
                    "{stem} ({os}): missing {:?}, unknown skips {:?}, skipped and covered {:?}",
                    r.missing.iter().map(|s| s.name()).collect::<Vec<_>>(),
                    r.unknown_skips,
                    r.skipped_and_covered
                        .iter()
                        .map(|s| s.name())
                        .collect::<Vec<_>>()
                ));
            }
        }
        checked += 1;
    }
    eprintln!("{checked} scenario(s) checked against their manifests");
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn test_names_replace_non_identifier_characters() {
    assert_eq!(
        test_name(8, "plannotator__herdr-annotate__annotate-links"),
        "plugin_08_plannotator__herdr_annotate__annotate_links"
    );
    assert_eq!(test_name(17, "0cv__a.b"), "plugin_17_0cv__a_b");
}

#[test]
fn scenario_keys_map_to_vibekes_grammar() {
    for (k, want) in [
        ("Enter", "enter"),
        ("Escape", "escape"),
        ("Tab", "tab"),
        ("C-c", "ctrl+c"),
        ("M-x", "alt+x"),
        ("C-M-Left", "ctrl+alt+left"),
        ("q", "q"),
        ("P", "P"),
        ("ctrl+c", "ctrl+c"),
    ] {
        assert_eq!(vibeke_key(k), want, "{k}");
    }
}

#[test]
fn the_environment_allowlist_never_passes_secrets() {
    for n in [
        "GITHUB_TOKEN",
        "SSH_AUTH_SOCK",
        "AWS_ACCESS_KEY_ID",
        "NPM_TOKEN",
        "API_KEY",
    ] {
        assert!(secret_like(n), "{n}");
    }
    for n in ENV_ALLOW {
        assert!(!secret_like(n), "{n}");
    }
    // The dependency proxy reaches plugin processes (18 §7.2).
    for n in [
        "HTTPS_PROXY",
        "https_proxy",
        "NO_PROXY",
        "SSL_CERT_FILE",
        "NODE_EXTRA_CA_CERTS",
        "GOPROXY",
    ] {
        assert!(ENV_ALLOW.contains(&n), "{n}");
    }

    // What plugin processes get from a parent environment full of secrets.
    let parent: BTreeMap<&str, &str> = [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", "/home/runner"),
        ("GITHUB_TOKEN", "ghs_x"),
        ("SSH_AUTH_SOCK", "/tmp/agent"),
        ("AWS_SECRET_ACCESS_KEY", "x"),
        ("HTTPS_PROXY", "http://127.0.0.1:8080"),
        ("http_proxy", "http://user:pass@proxy:3128"),
        ("NODE_EXTRA_CA_CERTS", "/ca.pem"),
        (TOOLCHAIN_PATH_VAR, "/tc/node/bin:/tc/go/bin"),
    ]
    .into_iter()
    .collect();
    let get = |n: &str| parent.get(n).map(|v| v.to_string());
    // Extra names from `VIBEKE_HERDR_PLUGINS_ENV_ALLOW` that look secret, or that would
    // replace the fresh HOME, are still dropped.
    let names: Vec<String> = ENV_ALLOW
        .iter()
        .map(|s| s.to_string())
        .chain(["GITHUB_TOKEN".to_string(), "HOME".to_string()])
        .collect();
    let env: BTreeMap<String, String> = plugin_env(&names, get, &[]).into_iter().collect();
    for n in [
        "GITHUB_TOKEN",
        "SSH_AUTH_SOCK",
        "AWS_SECRET_ACCESS_KEY",
        "http_proxy",
        "HOME",
    ] {
        assert!(!env.contains_key(n), "{n} must not reach plugins");
    }
    assert_eq!(env["HTTPS_PROXY"], "http://127.0.0.1:8080");
    assert_eq!(env["NODE_EXTRA_CA_CERTS"], "/ca.pem");
    assert_eq!(env["PATH"], "/tc/node/bin:/tc/go/bin:/usr/bin:/bin");
    for (k, v) in &env {
        assert!(!secret_like(k), "{k}");
        assert!(!v.contains("ghs_") && !v.contains("pass@"), "{k}={v}");
    }
    // The Herdr side puts Herdr's directory after the toolchains.
    let env: BTreeMap<String, String> = plugin_env(&names, get, &["/opt/herdr".to_string()])
        .into_iter()
        .collect();
    assert_eq!(
        env["PATH"],
        "/tc/node/bin:/tc/go/bin:/opt/herdr:/usr/bin:/bin"
    );
    // Without a toolchain path or an inherited PATH, plugins get a system PATH.
    let env: BTreeMap<String, String> = plugin_env(&names, |_| None, &[]).into_iter().collect();
    assert_eq!(env["PATH"], "/usr/bin:/bin:/usr/sbin:/sbin");
}

#[test]
fn proxy_urls_with_credentials_are_found() {
    assert!(has_url_credentials("http://user:pass@proxy:3128"));
    assert!(has_url_credentials("user@proxy:3128"));
    assert!(has_url_credentials("http://a:1, http://u@b:2"));
    assert!(!has_url_credentials("http://127.0.0.1:8080"));
    assert!(!has_url_credentials("http://proxy:8080/path@x"));
    assert!(!has_url_credentials("localhost,.internal"));
}

#[test]
fn herdr_plugin_dirs_use_plain_ids_only() {
    assert_eq!(
        herdr_path_component("zenbu.terminal-browser").as_deref(),
        Some("zenbu.terminal-browser")
    );
    assert_eq!(herdr_path_component("Acme.Tool"), None);
    assert_eq!(herdr_path_component("acme."), None);
    assert_eq!(herdr_path_component("con.tool"), None);
    assert_eq!(herdr_path_component("com1"), None);
    assert_eq!(
        herdr_path_component("community.tool").as_deref(),
        Some("community.tool")
    );
    assert_eq!(herdr_path_component(""), None);
}

#[test]
fn the_herdr_pane_open_reply_names_the_pane() {
    let out = r#"{"id":"cli","result":{"type":"plugin_pane_opened","plugin_pane":{"plugin_id":"acme.tool","entrypoint":"main","pane":{"pane_id":"w1-3","workspace_id":"w1"}}}}"#;
    assert_eq!(
        herdr_opened_pane(&format!("{out}\n")),
        Some(("main".to_string(), "w1-3".to_string()))
    );
    assert_eq!(
        herdr_opened_pane(&format!("note\n{out}\n")),
        Some(("main".to_string(), "w1-3".to_string()))
    );
    assert_eq!(herdr_opened_pane("Opened.\n"), None);
}

#[test]
fn plugins_find_the_tools_their_installers_put_in_home() {
    let get = |n: &str| match n {
        "CARGO_HOME" => Some("/box/home/.cargo".to_string()),
        "GOPATH" => Some("/box/home/go".to_string()),
        _ => None,
    };
    let dirs = user_bin_dirs(Path::new("/s/home"), get, "linux");
    assert_eq!(
        dirs,
        [
            "/s/home/.local/bin",
            "/s/home/.cargo/bin",
            "/s/home/go/bin",
            "/s/home/.bun/bin",
            "/box/home/.cargo/bin",
            "/box/home/go/bin",
        ]
    );
    let mac = user_bin_dirs(Path::new("/s/home"), |_| None, "macos");
    assert_eq!(&mac[4..], ["/opt/homebrew/bin", "/usr/local/bin"]);
    // After the toolchains, before the inherited PATH, without duplicates.
    let parent: BTreeMap<&str, &str> = [
        ("PATH", "/usr/local/bin:/usr/bin:/bin"),
        (TOOLCHAIN_PATH_VAR, "/tc/go/bin"),
    ]
    .into_iter()
    .collect();
    let env: BTreeMap<String, String> = plugin_env(
        &["PATH".to_string()],
        |n| parent.get(n).map(|v| v.to_string()),
        &mac,
    )
    .into_iter()
    .collect();
    assert_eq!(
        env["PATH"],
        "/tc/go/bin:/s/home/.local/bin:/s/home/.cargo/bin:/s/home/go/bin:/s/home/.bun/bin:\
         /opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"
    );
}

#[test]
fn step_reasons_keep_both_sides_and_fit_the_report() {
    assert_eq!(
        combine_reason(
            Some("Vibeke failed and Herdr has not run the step"),
            Some("no evidence for [Pane]"),
            Some("Herdr could not install the plugin"),
        )
        .as_deref(),
        Some(
            "Vibeke failed and Herdr has not run the step; vibeke: no evidence for [Pane]; \
             herdr: Herdr could not install the plugin"
        )
    );
    // Rule 4 already quotes Herdr's reason.
    assert_eq!(
        combine_reason(Some("Herdr failed too: boom"), Some("x"), Some("boom")).as_deref(),
        Some("Herdr failed too: boom; vibeke: x")
    );
    assert_eq!(combine_reason(None, None, None), None);
    let (a, b, c) = ("a".repeat(10_000), "b".repeat(10_000), "c".repeat(10_000));
    let r = combine_reason(Some(&a), Some(&b), Some(&c)).unwrap();
    assert!(r.encode_utf16().count() <= 1800, "{}", r.len());
    assert!(r.contains("; vibeke: ") && r.contains("; herdr: "));
}

#[test]
fn herdr_cli_arguments_follow_herdr_0_9_3() {
    assert_eq!(
        herdr_install_args("acme/tools", Some("plugins/one"), "abc"),
        [
            "plugin",
            "install",
            "acme/tools/plugins/one",
            "--ref",
            "abc",
            "--yes"
        ]
    );
    assert_eq!(
        herdr_install_args("acme/tools", None, "abc")[2],
        "acme/tools"
    );
    let open = |placement| herdr_pane_open_args("acme.tool", "main", placement, Some("w1-1"));
    assert_eq!(
        open(Some("split")),
        [
            "plugin",
            "pane",
            "open",
            "--plugin",
            "acme.tool",
            "--entrypoint",
            "main",
            "--target-pane",
            "w1-1"
        ]
    );
    assert!(open(Some("zoomed")).contains(&"--target-pane".to_string()));
    // Herdr refuses a target for overlay and popup panes, and for tabs.
    for p in [Some("overlay"), Some("popup"), Some("tab"), None] {
        assert_eq!(open(p).len(), 7, "{p:?}");
    }
}

#[test]
fn the_fake_agent_reports_with_a_source_on_both_sides() {
    // Herdr 0.9.3 rejects `pane report-agent` without `--source` ("missing required --source").
    let a = fake_agent_report_args("w1:p2", "idle");
    assert_eq!(
        a,
        [
            "pane",
            "report-agent",
            "w1:p2",
            "--source",
            "vibeke-harness",
            "--agent",
            "claude",
            "--state",
            "idle"
        ]
    );
    let help = std::fs::read_to_string(
        real_plugins::workspace_root()
            .join("tests/compat/herdr/0.9.3/cli-help/pane_report-agent.txt"),
    )
    .unwrap();
    for flag in ["--source", "--agent", "--state"] {
        assert!(help.contains(flag), "{flag} is not in Herdr's help");
    }
}

#[test]
fn stub_requests_match_the_raw_body() {
    // The sink stores the body as a JSON string; a `contains` with quotes matches the body as
    // the plugin sent it (heeler's push request).
    let body = r#"{"token":"0123456789abcdef","title":"blocked"}"#;
    let line = json!({"method": "POST", "path": "/push", "body": body}).to_string();
    assert!(line.contains(r#"\"token\""#));
    assert!(sink_request_matches(
        &line,
        Some(r#""token":"0123456789abcdef""#)
    ));
    assert!(sink_request_matches(&line, Some("/push")));
    assert!(sink_request_matches(&line, None));
    assert!(!sink_request_matches(&line, Some(r#""token":"other""#)));
    assert!(!sink_request_matches("not json", None));
}

#[test]
fn hook_output_is_read_as_text() {
    // herdr-plus prints a quoted branch name; the record holds it as a JSON string.
    let l = json!({
        "stdout": "herdr-plus: applied worktree layout \"app.toml\" to repo \"app\" (branch \"feat/layout\"): 2 tab(s).\n",
        "stderr": "",
    });
    assert!(log_output(&l).contains(r#"branch "feat/layout"): 2 tab(s)"#));
    assert_eq!(log_output(&json!({"stdout": null, "stderr": "e"})), "e");
}

#[test]
fn a_failed_action_invocation_ends_the_step_at_once() {
    let logs = vec![
        json!({"log_id": "h1", "event": "pane.created", "status": "succeeded"}),
        json!({"log_id": "a1", "action_id": "open", "status": "failed", "exit_code": 1,
               "stderr": format!("{}\nopen: no directory context", "x".repeat(2000))}),
    ];
    let e = final_invocation(&logs, "succeeded", Some("open")).unwrap_err();
    assert!(e.contains("a1 failed (exit code 1)"), "{e}");
    assert!(e.contains("no directory context"), "{e}");
    assert!(e.len() < 1000, "{}", e.len());
    // A qualified id (Herdr) is the same action.
    let ok = vec![json!({"log_id": "a2", "action_id": "acme.open", "status": "succeeded"})];
    assert_eq!(
        final_invocation(&ok, "succeeded", Some("open"))
            .unwrap()
            .unwrap()["log_id"],
        "a2"
    );
    // Still running: wait.
    let running = vec![json!({"log_id": "a3", "action_id": "open", "status": "running"})];
    assert!(
        final_invocation(&running, "succeeded", Some("open"))
            .unwrap()
            .is_none()
    );
    // Without an action, any new invocation counts and nothing fails early.
    assert!(
        final_invocation(&logs, "succeeded", None)
            .unwrap()
            .is_some()
    );
    assert!(
        final_invocation(&logs[1..], "succeeded", None)
            .unwrap()
            .is_none()
    );
    let d = invocation_details(
        &json!({"log_id": "z", "status": "failed", "exit_code": null,
                                        "error": "spawn failed", "stdout": "out"}),
    );
    assert_eq!(
        d,
        "invocation z failed (exit code null); error: spawn failed; stdout tail: out"
    );
}

#[test]
fn launcher_failures_show_the_command() {
    let line = vk_conformance::trace::TraceLine {
        ts_ms: 0,
        plugin: "p".into(),
        invocation: None,
        transport: "launcher".into(),
        method: String::new(),
        ok: false,
        code: Some("usage".into()),
        params: None,
        argv: Some(vec![
            "status".into(),
            "server".into(),
            "--json".into(),
            "x".into(),
            "y".into(),
        ]),
        parse_error: Some("unknown noun `status`".into()),
    };
    assert_eq!(
        describe_failed_call(&line),
        "unexpected failed herdr command `status server --json x` (usage): unknown noun `status`"
    );
    let socket = vk_conformance::trace::TraceLine {
        transport: "socket".into(),
        method: "pane.layout".into(),
        code: Some("method_not_found".into()),
        argv: None,
        parse_error: None,
        ..line
    };
    assert_eq!(
        describe_failed_call(&socket),
        "unexpected failed call pane.layout (method_not_found)"
    );
}

#[test]
fn screenshots_cover_the_panes_a_step_opens() {
    let sc: Scenario = toml::from_str(
        r#"
plugin_id = "acme.tool"
repository = "acme/tool"

[[step]]
id = "open"
kind = "pane"
pane = "main"
expect = [{ kind = "pane", pane = "main" }, { kind = "popup", pane = "picker" }]

[[step]]
id = "type"
kind = "input"
pane = "picker"
keys = ["Enter"]
expect = [{ kind = "screen_text", pane = "status", text = "done" }]
"#,
    )
    .unwrap();
    assert_eq!(shot_panes(&sc.steps[0]), ["main", "picker"]);
    // A failed step keeps the grid of every pane its expectations name (H12).
    assert_eq!(shot_panes(&sc.steps[1]), ["picker", "status"]);
}

#[test]
fn the_deferred_plugins_keep_their_slots() {
    let lock = real_plugins::load_lock();
    let deferred: Vec<&str> = lock.defer.iter().map(|d| d.repository.as_str()).collect();
    for repo in [
        "openclaw/crabbox",
        "zenbu-labs/terminal-browser",
        "zenbu-labs/terminal-code",
        "powerfooI/roamgate",
        "devswha/herdr-web-ui",
    ] {
        assert!(deferred.contains(&repo), "{repo} is not deferred");
        // Still locked: the next catalog entry does not take the slot.
        assert!(
            lock.plugins.iter().any(|p| p.repository == repo),
            "{repo} lost its slot"
        );
    }
    for d in &lock.defer {
        assert!(!d.reason.trim().is_empty(), "{}", d.repository);
    }
}

#[test]
fn the_vibeke_side_starts_without_onboarding_or_update_checks() {
    let text = vibeke_config();
    let cfg: toml::Value = toml::from_str(&text).unwrap();
    assert_eq!(cfg["onboarding"].as_bool(), Some(false));
    assert_eq!(cfg["update"]["version_check"].as_bool(), Some(false));
    assert_eq!(cfg["update"]["manifest_check"].as_bool(), Some(false));
    assert_eq!(
        cfg["ui"]["sidebar"]["width"].as_integer(),
        Some(vk_conformance::herdr::SIDEBAR_WIDTH as i64)
    );
    assert_eq!(cfg["ui"]["sidebar"]["auto_width"].as_bool(), Some(false));
    // The same file must load as a Vibeke config, without warnings about unknown keys.
    let (_, warnings) = vk_config::Config::parse(&text, Path::new("config.toml")).unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");
}

#[test]
fn both_sides_get_the_same_invocation_context() {
    let ctx = |k| invocation_context(k, "w1", "w1:t1", "w1:p1", Some("w1:p2"));
    assert_eq!(ctx(None), json!({}));
    assert_eq!(ctx(Some("global")), json!({}));
    assert_eq!(ctx(Some("workspace")), json!({"workspace_id": "w1"}));
    assert_eq!(
        ctx(Some("tab")),
        json!({"workspace_id": "w1", "tab_id": "w1:t1"})
    );
    assert_eq!(
        ctx(Some("pane")),
        json!({"workspace_id": "w1", "tab_id": "w1:t1", "focused_pane_id": "w1:p1"})
    );
    assert_eq!(
        ctx(Some("agent_pane")),
        json!({"workspace_id": "w1", "tab_id": "w1:t1", "focused_pane_id": "w1:p2"})
    );
}

#[test]
fn herdr_response_lines_are_unwrapped() {
    assert_eq!(
        herdr_result(json!({"id": "cli", "result": {"type": "pong"}})),
        json!({"type": "pong"})
    );
    assert_eq!(
        herdr_result(json!({"result": {"type": "pong"}})),
        json!({"type": "pong"})
    );
    // The bare result of an older shim stays as it is.
    let bare = json!({"workspace": {"workspace_id": "w1"}, "result": 1, "extra": 2});
    assert_eq!(herdr_result(bare.clone()), bare);
    assert_eq!(
        herdr_result(json!({"workspace": {"workspace_id": "w1"}})),
        json!({"workspace": {"workspace_id": "w1"}})
    );
}

#[test]
fn links_are_found_on_screen_but_not_in_the_command_line() {
    let url = "https://github.com/a/b";
    let screen =
        "$ clear; printf '%s%s\\n' 'https:' '//github.com/a/b'\nhttps://github.com/a/b\n$ ";
    assert_eq!(link_position(screen, url), Some((1, 1)));
    assert_eq!(
        link_position("x 'https://github.com/a/b'\n  https://github.com/a/b", url),
        Some((1, 3))
    );
    assert_eq!(link_position("nothing here", url), None);
    assert_eq!(sh_quote("it's"), "'it'\\''s'");
}

#[test]
fn links_without_a_scheme_or_wrapped_are_found() {
    // A plain path (herdr-nvim's `file-path` handler) is printed in two parts, too.
    let path = "~/src/app/src/notes.rs:2";
    let (a, b) = link_print_parts(path).unwrap();
    assert_eq!((a.as_str(), b.as_str()), ("~", "/src/app/src/notes.rs:2"));
    let cmd = format!(
        "$ clear; printf '%s%s\\n' {} {}",
        sh_quote(&a),
        sh_quote(&b)
    );
    assert!(!cmd.contains(path));
    assert_eq!(
        link_position(&format!("{cmd}\n{path}\n$ "), path),
        Some((1, 1))
    );
    assert_eq!(
        link_print_parts("https://a/b"),
        Some(("https:".to_string(), "//a/b".to_string()))
    );
    assert_eq!(link_print_parts(""), None);
    // A 56-character URL in a 24-column pane wraps over three rows.
    let url = "file:///tmp/vkhrxXayEt/herdr/home/src/app/src/second.rs";
    let rows: Vec<String> = url
        .chars()
        .collect::<Vec<_>>()
        .chunks(24)
        .map(|c| c.iter().collect())
        .collect();
    let screen = format!("$ clear; printf ...\n{}\n$ ", rows.join("\n"));
    assert_eq!(link_position(&screen, url), Some((1, 1)));
    // It may start in the middle of a row.
    let screen = format!("> {}\n{}", &url[..22], &url[22..]);
    assert_eq!(link_position(&screen, url), Some((0, 3)));
    // A row that only starts like the URL is not a match.
    assert_eq!(link_position("x file:///tmp\nsomething else", url), None);
    assert_eq!(
        link_position("'file:///tmp/vkhr\nxXayEt", "file:///tmp/vkhrxXayEt"),
        None
    );
}

#[test]
fn github_rate_limits_are_recognised() {
    assert!(rate_limited(
        "curl: (22) The requested URL returned error: 403 from api.github.com"
    ));
    assert!(rate_limited("API rate limit exceeded (403)"));
    assert!(!rate_limited("npm ERR! 404 Not Found"));
    assert!(!rate_limited("compile error"));
}

#[test]
fn excluded_dirs_leave_the_plugin_path() {
    let mut env = vec![
        ("HOME".to_string(), "/h".to_string()),
        (
            "PATH".to_string(),
            "/tc/bin:/opt/homebrew/bin:/usr/bin:/opt/homebrew/bin/".to_string(),
        ),
    ];
    without_path_dirs(&mut env, &["/opt/homebrew/bin/".to_string()]);
    assert_eq!(env[1].1, "/tc/bin:/usr/bin");
    assert_eq!(env[0].1, "/h");
    without_path_dirs(&mut env, &[]);
    assert_eq!(env[1].1, "/tc/bin:/usr/bin");
}

#[test]
fn processes_are_found_by_the_paths_they_use() {
    assert!(names_path_under("/tmp/vkhr1/home", "/tmp/vkhr1/home"));
    assert!(names_path_under(
        "/tc/bin:/tmp/vkhr1/home/.local/bin:/usr/bin",
        "/tmp/vkhr1/home"
    ));
    // The Herdr side's dirs are not under the Vibeke side's HOME.
    assert!(!names_path_under(
        "/tmp/vkhr1/herdr/home",
        "/tmp/vkhr1/home"
    ));
    assert!(!names_path_under("/tmp/vkhr1/homework", "/tmp/vkhr1/home"));
    assert!(!names_path_under("/anything", ""));
}

#[test]
fn traced_pane_opens_name_their_entrypoint() {
    let line = |params: Option<serde_json::Value>| vk_conformance::trace::TraceLine {
        ts_ms: 0,
        plugin: "unknown".into(),
        invocation: None,
        transport: "socket".into(),
        method: "plugin.pane.open".into(),
        ok: true,
        code: None,
        params,
        argv: None,
        parse_error: None,
    };
    assert_eq!(
        popup_open_entrypoint(
            &line(Some(
                json!({"plugin_id": "acme.tool", "entrypoint": "pick"})
            )),
            "acme.tool"
        )
        .as_deref(),
        Some("pick")
    );
    assert_eq!(
        popup_open_entrypoint(
            &line(Some(json!({"plugin_id": "other", "entrypoint": "pick"}))),
            "acme.tool"
        ),
        None
    );
    assert_eq!(
        popup_open_entrypoint(&line(Some(json!({"entrypoint": "pick"}))), "acme.tool").as_deref(),
        Some("pick")
    );
    assert_eq!(popup_open_entrypoint(&line(None), "acme.tool"), None);
}

/// The gate refuses before any plugin code runs when the variable is missing.
#[test]
fn without_the_variable_the_plugin_tests_refuse() {
    if std::env::var("VIBEKE_HERDR_PLUGINS").as_deref() == Ok("1") {
        return;
    }
    let r = std::panic::catch_unwind(real_plugins::gate);
    let msg = r
        .err()
        .and_then(|e| {
            e.downcast_ref::<String>()
                .cloned()
                .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
        })
        .unwrap_or_default();
    assert!(msg.contains("VIBEKE_HERDR_PLUGINS=1"), "{msg}");
}
