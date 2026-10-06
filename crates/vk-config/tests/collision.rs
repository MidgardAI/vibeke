//! `[collision]` (05 §10): typed keys with located warnings and per-key defaults.

use std::path::Path;
use std::time::Duration;
use vk_config::{Collision, Config, FsAttribution};

fn parse(src: &str) -> (Config, Vec<vk_config::Warning>) {
    Config::parse(src, Path::new("test.toml")).unwrap()
}

#[test]
fn defaults_are_the_spec_values() {
    let c = Collision::default();
    assert!(c.enabled);
    assert_eq!(c.window, Duration::from_secs(1800));
    assert_eq!(c.read_window, Duration::from_secs(600));
    assert_eq!(c.fs_attribution, FsAttribution::Auto);
    assert!(!c.enforce_claims);
    let (cfg, w) = parse("");
    assert_eq!(w, vec![]);
    assert_eq!(cfg.collision(), c);
}

#[test]
fn the_section_is_external_and_every_documented_key_parses() {
    let (cfg, w) = parse(
        r#"
[collision]
enabled = true
window = "10m"
read_window = "2m"
fs_attribution = "aggressive"
enforce_claims = true
dir_depth = 3
poll_interval = "2s"
settle = "50ms"
watcher = "none"
notify = false
ignore = ["dist/**", "*.log"]
max_touches = 200
"#,
    );
    assert_eq!(w, vec![]);
    assert!(cfg.extra.contains_key("collision"));
    let c = cfg.collision();
    assert_eq!(c.window, Duration::from_secs(600));
    assert_eq!(c.read_window, Duration::from_secs(120));
    assert_eq!(c.fs_attribution, FsAttribution::Aggressive);
    assert!(c.enforce_claims);
    assert_eq!(c.dir_depth, 3);
    assert_eq!(c.poll_interval, Duration::from_secs(2));
    assert_eq!(c.settle, Duration::from_millis(50));
    assert_eq!(c.watcher, "none");
    assert!(!c.notify);
    assert_eq!(c.ignore, vec!["dist/**".to_string(), "*.log".to_string()]);
    assert_eq!(c.max_touches, 200);
}

#[test]
fn a_bad_key_warns_and_keeps_only_its_own_default() {
    let (cfg, w) = parse(
        r#"
[collision]
window = "soon"
fs_attribution = "fanotify"
enforce_claims = true
dir_depth = -1
bogus = 1
"#,
    );
    let keys: Vec<&str> = w.iter().map(|w| w.key.as_str()).collect();
    for k in [
        "collision.window",
        "collision.fs_attribution",
        "collision.dir_depth",
        "collision.bogus",
    ] {
        assert!(keys.contains(&k), "{k} warned: {keys:?}");
    }
    let c = cfg.collision();
    assert_eq!(
        c.window,
        Duration::from_secs(1800),
        "bad window keeps default"
    );
    assert_eq!(c.fs_attribution, FsAttribution::Auto);
    assert!(c.enforce_claims, "the good key still applies");
    assert_eq!(c.dir_depth, 2);
}

#[test]
fn a_non_table_section_is_one_warning() {
    let (_cfg, w) = parse("collision = 3\n");
    assert!(w.iter().any(|w| w.key == "collision"));
}
