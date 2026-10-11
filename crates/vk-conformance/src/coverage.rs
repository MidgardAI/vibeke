//! The coverage check (18 §8.1): every action, event hook, pane, startup command and link
//! handler a manifest declares for the current platform appears in a scenario step, or in an
//! explicit `skip` with a reason. A missing entry fails the run.

use crate::scenario::{Expectation, Scenario, StepKind};
use std::collections::BTreeSet;
use vk_compat::herdr::manifest::Manifest;

/// The host platforms the harness runs on. A skip is stale only when no host platform
/// declares its surface, so one scenario serves Linux and macOS.
const HOST_PLATFORMS: [&str; 2] = ["linux", "macos"];

/// One declared surface. [`Surface::name`] is the spelling `skip.surface` uses.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Surface {
    /// `action:<id>`
    Action(String),
    /// `event:<on>`; one per distinct event name.
    Event(String),
    /// `pane:<id>`
    Pane(String),
    /// `startup:<index>` (0-based, in manifest order)
    Startup(usize),
    /// `link:<id>`
    Link(String),
}

impl Surface {
    pub fn name(&self) -> String {
        match self {
            Surface::Action(id) => format!("action:{id}"),
            Surface::Event(on) => format!("event:{on}"),
            Surface::Pane(id) => format!("pane:{id}"),
            Surface::Startup(i) => format!("startup:{i}"),
            Surface::Link(id) => format!("link:{id}"),
        }
    }
}

/// The surfaces `manifest` declares for `os` (`"linux"` or `"macos"`), honouring per-entry
/// `platforms`. An entry without its own `platforms` inherits the plugin's list, as in
/// [`Manifest::entry_on`]. Startup indexes count every `[[startup]]` entry, so an index names
/// the same entry on every platform. The list is sorted and has no duplicates.
pub fn declared(manifest: &Manifest, os: &str) -> Vec<Surface> {
    let on = |p: Option<&Vec<String>>| manifest.entry_on(p, os);
    let mut out = BTreeSet::new();
    for a in manifest.actions.iter().filter(|a| on(a.platforms.as_ref())) {
        out.insert(Surface::Action(a.id.clone()));
    }
    for e in manifest.events.iter().filter(|e| on(e.platforms.as_ref())) {
        out.insert(Surface::Event(e.on.clone()));
    }
    for p in manifest.panes.iter().filter(|p| on(p.platforms.as_ref())) {
        out.insert(Surface::Pane(p.id.clone()));
    }
    for (i, _) in manifest
        .startup
        .iter()
        .enumerate()
        .filter(|(_, s)| on(s.platforms.as_ref()))
    {
        out.insert(Surface::Startup(i));
    }
    for l in manifest
        .link_handlers
        .iter()
        .filter(|l| on(l.platforms.as_ref()))
    {
        out.insert(Surface::Link(l.id.clone()));
    }
    out.into_iter().collect()
}

/// Which surface a step covers, if any. An action step covers `action:<id>`; an event step
/// covers `event:<event>`; a pane step or a `pane`/`popup` expectation covers `pane:<id>`; a
/// link step covers every link handler whose pattern matches its URL; a `ready` expectation, or
/// any step after a successful activation, covers the startup commands.
///
/// The startup rule in detail: every `[[startup]]` entry is covered when a step has a `ready`
/// expectation, or when the manifest has startup entries and the scenario has at least one
/// action, pane or event step (running one needs an activated plugin, and activation runs the
/// startup commands). An action id may be qualified (`<plugin-id>.<action>`); it is resolved to
/// the bare id. Link patterns are matched the way the server does: the manifest's regular
/// expression must match somewhere in the URL. The result is sorted and has no duplicates, and
/// it may name surfaces that the manifest does not declare on this platform.
pub fn covered(scenario: &Scenario, manifest: &Manifest) -> Vec<Surface> {
    let mut out = BTreeSet::new();
    let mut ready = false;
    let mut activated = false;
    for step in &scenario.steps {
        match step.kind {
            StepKind::Action => {
                if let Some(a) = &step.action {
                    let id = manifest
                        .resolve_action(a)
                        .map_or_else(|| a.clone(), |x| x.id.clone());
                    out.insert(Surface::Action(id));
                }
                activated = true;
            }
            StepKind::Event => {
                if let Some(e) = &step.event {
                    out.insert(Surface::Event(e.clone()));
                }
                activated = true;
            }
            StepKind::Pane => {
                if let Some(p) = &step.pane {
                    out.insert(Surface::Pane(p.clone()));
                }
                activated = true;
            }
            StepKind::Link => {
                if let Some(url) = &step.url {
                    for l in &manifest.link_handlers {
                        if regex::Regex::new(&l.pattern).is_ok_and(|re| re.is_match(url)) {
                            out.insert(Surface::Link(l.id.clone()));
                        }
                    }
                }
            }
            StepKind::Restart | StepKind::Lifecycle | StepKind::Input | StepKind::Wait => {}
        }
        for e in &step.expect {
            match e {
                Expectation::Pane { pane, .. } | Expectation::Popup { pane } => {
                    out.insert(Surface::Pane(pane.clone()));
                }
                Expectation::Ready { .. } => ready = true,
                _ => {}
            }
        }
    }
    if ready || (activated && !manifest.startup.is_empty()) {
        out.extend((0..manifest.startup.len()).map(Surface::Startup));
    }
    out.into_iter().collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CoverageReport {
    /// Declared, but neither covered nor skipped.
    pub missing: Vec<Surface>,
    /// Skipped surfaces that the manifest does not declare (stale skips also fail).
    pub unknown_skips: Vec<String>,
    /// Skipped and also covered (a contradiction that fails too).
    pub skipped_and_covered: Vec<Surface>,
}

impl CoverageReport {
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
            && self.unknown_skips.is_empty()
            && self.skipped_and_covered.is_empty()
    }
}

/// Compare the scenario with what the manifest declares for `os`. A skip is unknown only when
/// neither Linux nor macOS (nor `os`) declares its surface, so a scenario can skip a surface
/// that exists on one platform only.
pub fn check(scenario: &Scenario, manifest: &Manifest, os: &str) -> CoverageReport {
    let declared_here = declared(manifest, os);
    let covered: BTreeSet<Surface> = covered(scenario, manifest).into_iter().collect();
    let skipped: BTreeSet<&str> = scenario.skips.iter().map(|s| s.surface.as_str()).collect();
    let platforms = [HOST_PLATFORMS[0], HOST_PLATFORMS[1], os];
    let anywhere: BTreeSet<String> = platforms
        .into_iter()
        .flat_map(|pf| declared(manifest, pf))
        .map(|s| s.name())
        .collect();

    let missing = declared_here
        .iter()
        .filter(|s| !covered.contains(*s) && !skipped.contains(s.name().as_str()))
        .cloned()
        .collect();
    let unknown_skips = skipped
        .iter()
        .filter(|s| !anywhere.contains(**s))
        .map(|s| s.to_string())
        .collect();
    let skipped_and_covered = covered
        .iter()
        .filter(|s| skipped.contains(s.name().as_str()))
        .cloned()
        .collect();
    CoverageReport {
        missing,
        unknown_skips,
        skipped_and_covered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MANIFEST: &str = r#"
id = "acme.demo"
platforms = ["linux", "macos"]

[[startup]]
command = ["bin/serve"]

[[startup]]
platforms = ["macos"]
command = ["bin/mac-only"]

[[actions]]
id = "open"
title = "Open"
command = ["bin/demo", "open"]

[[actions]]
id = "doctor"
command = ["bin/demo", "doctor"]

[[actions]]
id = "linux-only"
platforms = ["linux"]
command = ["bin/demo", "x"]

[[events]]
on = "worktree.created"
command = ["bin/demo", "auto"]

[[events]]
on = "worktree.created"
command = ["bin/demo", "auto2"]

[[panes]]
id = "main"
placement = "popup"
command = ["bin/demo"]

[[link_handlers]]
id = "gh"
title = "GitHub"
pattern = '^https://github\.com/'
action = "acme.demo.open"

[[link_handlers]]
id = "gl"
title = "GitLab"
pattern = 'gitlab\.com'
action = "open"
"#;

    fn manifest() -> Manifest {
        Manifest::parse(MANIFEST).unwrap()
    }

    fn scenario(body: &str) -> Scenario {
        let text = format!("plugin_id = \"acme.demo\"\nrepository = \"acme/demo\"\n{body}");
        let s: Scenario = toml::from_str(&text).unwrap();
        s.validate().unwrap();
        s
    }

    const FULL: &str = r#"
[[step]]
id = "open"
kind = "action"
action = "acme.demo.open"
[[step.expect]]
kind = "popup"
pane = "main"

[[step]]
id = "link"
kind = "link"
url = "https://github.com/a/b"

[[step]]
id = "event"
kind = "event"
event = "worktree.created"
cli = ["worktree", "create"]

[[skip]]
surface = "action:doctor"
reason = "needs a cloud account"

[[skip]]
surface = "action:linux-only"
reason = "not useful in tests"

[[skip]]
surface = "link:gl"
reason = "no GitLab"
"#;

    #[test]
    fn names() {
        assert_eq!(Surface::Action("a".into()).name(), "action:a");
        assert_eq!(Surface::Event("x.y".into()).name(), "event:x.y");
        assert_eq!(Surface::Pane("p".into()).name(), "pane:p");
        assert_eq!(Surface::Startup(2).name(), "startup:2");
        assert_eq!(Surface::Link("l".into()).name(), "link:l");
    }

    #[test]
    fn declared_honours_platforms() {
        let m = manifest();
        let linux = declared(&m, "linux");
        assert!(linux.contains(&Surface::Action("linux-only".into())));
        assert!(linux.contains(&Surface::Startup(0)));
        assert!(!linux.contains(&Surface::Startup(1)));
        let mac = declared(&m, "macos");
        assert!(!mac.contains(&Surface::Action("linux-only".into())));
        assert!(mac.contains(&Surface::Startup(1)));
        assert_eq!(
            linux
                .iter()
                .filter(|s| matches!(s, Surface::Event(_)))
                .count(),
            1,
            "one surface per event name"
        );
        let mut sorted = linux.clone();
        sorted.sort();
        assert_eq!(linux, sorted);
        assert!(declared(&m, "windows").is_empty());
    }

    #[test]
    fn full_scenario_is_covered_on_both_platforms() {
        let m = manifest();
        let s = scenario(FULL);
        for os in ["linux", "macos"] {
            let r = check(&s, &m, os);
            assert!(r.ok(), "{os}: {r:?}");
        }
        let c = covered(&s, &m);
        assert!(c.contains(&Surface::Action("open".into())), "qualified id");
        assert!(c.contains(&Surface::Link("gh".into())));
        assert!(!c.contains(&Surface::Link("gl".into())));
        assert!(c.contains(&Surface::Pane("main".into())));
        assert!(c.contains(&Surface::Startup(0)) && c.contains(&Surface::Startup(1)));
    }

    #[test]
    fn missing_surfaces_fail() {
        let m = manifest();
        let s = scenario(&FULL.replace("action = \"acme.demo.open\"", "action = \"doctor\""));
        let r = check(&s, &m, "linux");
        assert!(!r.ok());
        assert_eq!(r.missing, vec![Surface::Action("open".into())]);
        assert_eq!(
            r.skipped_and_covered,
            vec![Surface::Action("doctor".into())]
        );
    }

    #[test]
    fn startup_needs_activation_or_ready() {
        let m = manifest();
        let s = scenario(
            r#"
[[step]]
id = "link"
kind = "link"
url = "https://github.com/a/b"
"#,
        );
        let r = check(&s, &m, "linux");
        assert!(r.missing.contains(&Surface::Startup(0)));
        let s = scenario(
            r#"
[[step]]
id = "wait"
kind = "wait"
[[step.expect]]
kind = "ready"
text = "listening"
"#,
        );
        assert!(covered(&s, &m).contains(&Surface::Startup(0)));
        let no_startup =
            Manifest::parse("id = \"a.b\"\n[[actions]]\nid = \"x\"\ncommand = [\"x\"]\n").unwrap();
        let s = scenario("[[step]]\nid = \"x\"\nkind = \"action\"\naction = \"x\"\n");
        assert_eq!(covered(&s, &no_startup), vec![Surface::Action("x".into())]);
        assert!(check(&s, &no_startup, "linux").ok());
    }

    #[test]
    fn stale_and_platform_skips() {
        let m = manifest();
        let s = scenario(&format!(
            "{FULL}\n[[skip]]\nsurface = \"action:gone\"\nreason = \"removed\"\n"
        ));
        let r = check(&s, &m, "macos");
        assert_eq!(r.unknown_skips, vec!["action:gone".to_string()]);
        assert!(r.missing.is_empty());
        // `action:linux-only` is skipped on macOS too, where it is not declared: not stale.
        assert!(!r.unknown_skips.contains(&"action:linux-only".to_string()));
    }

    #[test]
    fn pane_expectation_covers_pane() {
        let m = manifest();
        let s = scenario(
            r#"
[[step]]
id = "p"
kind = "pane"
pane = "main"
"#,
        );
        assert!(covered(&s, &m).contains(&Surface::Pane("main".into())));
    }
}
