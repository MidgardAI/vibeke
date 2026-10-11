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

use crate::support::real_plugins::{self, ENV_ALLOW, secret_like, vibeke_key};
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
