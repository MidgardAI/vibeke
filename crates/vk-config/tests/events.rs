//! `[events]` (02 §2.3): typed retention keys with located warnings and per-key defaults.

use std::path::Path;
use vk_config::{Config, Events};

fn parse(src: &str) -> (Config, Vec<vk_config::Warning>) {
    Config::parse(src, Path::new("test.toml")).unwrap()
}

#[test]
fn defaults_are_the_spec_values() {
    let e = Events::default();
    assert_eq!(
        (e.sync_days, e.history_days, e.blob_days, e.max_rows),
        (7, 365, 30, 2_000_000)
    );
    let (cfg, warnings) = parse("");
    assert_eq!(warnings, vec![]);
    assert_eq!(cfg.events(), e);
}

#[test]
fn the_section_is_external_and_never_an_unknown_key() {
    let (cfg, warnings) = parse("[events]\nsync_retention = \"3d\"\n");
    assert_eq!(warnings, vec![]);
    assert!(cfg.extra.contains_key("events"));
    assert_eq!(cfg.events().sync_days, 3);
}

#[test]
fn durations_integers_and_the_cap_parse() {
    let (cfg, w) = parse(
        r#"
[events]
sync_retention = "36h"
history_retention = 90
blob_retention = "2w"
max_rows = 0
"#,
    );
    assert_eq!(w, vec![]);
    let e = cfg.events();
    assert_eq!(e.sync_days, 2, "36h rounds up to whole days");
    assert_eq!(e.history_days, 90);
    assert_eq!(e.blob_days, 14);
    assert_eq!(e.max_rows, 0);
}

#[test]
fn a_bad_key_warns_with_a_position_and_keeps_only_its_own_default() {
    let (cfg, w) = parse(
        r#"
[events]
sync_retention = "3d"
history_retention = "soon"
blob_retention = 0
max_rows = -5
sync_retension = "1d"
"#,
    );
    let e = cfg.events();
    assert_eq!(e.sync_days, 3, "the good key still applies");
    assert_eq!(e.history_days, 365);
    assert_eq!(e.blob_days, 30);
    assert_eq!(e.max_rows, 2_000_000);
    let keys: Vec<&str> = w.iter().map(|w| w.key.as_str()).collect();
    for k in [
        "events.history_retention",
        "events.blob_retention",
        "events.max_rows",
        "events.sync_retension",
    ] {
        assert!(keys.contains(&k), "{k} in {keys:?}");
    }
    assert!(w.iter().all(|w| w.pos.is_some()), "{w:?}");
}

#[test]
fn a_non_table_section_warns() {
    let (cfg, w) = parse("events = 5\n");
    assert_eq!(cfg.events(), Events::default());
    assert!(w.iter().any(|w| w.key == "events"), "{w:?}");
}
