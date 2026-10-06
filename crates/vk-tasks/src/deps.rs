//! Dependency strategy (05 §5 `[deps]`): share a warm `node_modules` with a
//! copy-on-write clone when the lockfile is identical, otherwise install.

use crate::clone::cow_available;
use crate::files::{CopyOutcome, CopyResult, expand_glob};
use crate::taskfile::{DepsSpec, DepsStrategy};
use serde::Serialize;
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PackageManager {
    Pnpm,
    Bun,
    Yarn,
    Npm,
    Uv,
    Poetry,
    Cargo,
    Go,
}

impl PackageManager {
    pub fn name(self) -> &'static str {
        match self {
            Self::Pnpm => "pnpm",
            Self::Bun => "bun",
            Self::Yarn => "yarn",
            Self::Npm => "npm",
            Self::Uv => "uv",
            Self::Poetry => "poetry",
            Self::Cargo => "cargo",
            Self::Go => "go",
        }
    }

    /// Default install command; `None` for managers that need none (cargo and
    /// go fetch on build).
    pub fn install_command(self) -> Option<&'static str> {
        match self {
            Self::Pnpm => Some("pnpm install --frozen-lockfile --prefer-offline"),
            Self::Bun => Some("bun install --frozen-lockfile"),
            Self::Yarn => Some("yarn install --frozen-lockfile"),
            Self::Npm => Some("npm ci --prefer-offline"),
            Self::Uv => Some("uv sync"),
            Self::Poetry => Some("poetry install"),
            Self::Cargo | Self::Go => None,
        }
    }

    pub fn lockfiles(self) -> &'static [&'static str] {
        match self {
            Self::Pnpm => &["pnpm-lock.yaml"],
            Self::Bun => &["bun.lock", "bun.lockb"],
            Self::Yarn => &["yarn.lock"],
            Self::Npm => &["package-lock.json", "npm-shrinkwrap.json"],
            Self::Uv => &["uv.lock"],
            Self::Poetry => &["poetry.lock"],
            Self::Cargo => &["Cargo.lock"],
            Self::Go => &["go.sum"],
        }
    }

    /// Whether the clone strategy applies (`node_modules` trees).
    fn clones_node_modules(self) -> bool {
        matches!(self, Self::Pnpm | Self::Bun | Self::Yarn | Self::Npm)
    }
}

/// Detect the package manager of the checkout at `root` from lockfiles (a
/// bare `package.json` means npm).
pub fn detect_package_manager(root: &Path) -> Option<PackageManager> {
    use PackageManager::*;
    let has = |f: &str| root.join(f).is_file();
    if has("pnpm-lock.yaml") {
        Some(Pnpm)
    } else if has("bun.lock") || has("bun.lockb") {
        Some(Bun)
    } else if has("yarn.lock") {
        Some(Yarn)
    } else if has("package-lock.json") || has("npm-shrinkwrap.json") {
        Some(Npm)
    } else if has("uv.lock") {
        Some(Uv)
    } else if has("poetry.lock") {
        Some(Poetry)
    } else if has("package.json") {
        Some(Npm)
    } else if has("Cargo.toml") {
        Some(Cargo)
    } else if has("go.mod") {
        Some(Go)
    } else {
        None
    }
}

/// What the dependency step does for a new task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum DepsAction {
    /// Clone these source-relative directories (copy-on-write, else copy).
    Clone {
        dirs: Vec<String>,
    },
    /// Run this command as the first setup step.
    Install {
        command: String,
    },
    None,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DepsPlan {
    pub strategy: DepsStrategy,
    pub manager: Option<PackageManager>,
    pub action: DepsAction,
    /// One line for the UI and events: why this action.
    pub reason: String,
}

impl DepsPlan {
    /// The install command to run, if the plan installs.
    pub fn install_command(&self) -> Option<&str> {
        match &self.action {
            DepsAction::Install { command } => Some(command),
            _ => None,
        }
    }
}

fn lock_hash(root: &Path, m: PackageManager) -> Option<String> {
    m.lockfiles().iter().find_map(|f| {
        fs::read(root.join(f))
            .ok()
            .map(|b| blake3::hash(&b).to_hex().to_string())
    })
}

/// `node_modules` directories present in the source checkout: the root one
/// and workspace packages up to two levels down (`apps/*/node_modules`).
fn node_module_dirs(src: &Path) -> Vec<String> {
    let mut v = Vec::new();
    for pat in ["node_modules", "*/*/node_modules"] {
        for rel in expand_glob(src, pat) {
            // Skip anything nested in another node_modules.
            let nested = rel.split('/').filter(|c| *c == "node_modules").count() > 1
                || (rel != "node_modules" && rel.starts_with("node_modules/"));
            if !nested && src.join(&rel).is_dir() {
                v.push(rel);
            }
        }
    }
    v.sort();
    v.dedup();
    v
}

/// Decide the dependency action. `src` is the source checkout, `dst` the new
/// worktree (its lockfile comes from git). Pure apart from reading files and
/// one copy-on-write probe.
pub fn plan_deps(spec: &DepsSpec, src: &Path, dst: &Path) -> DepsPlan {
    let strategy = spec.strategy.unwrap_or_default();
    let manager = detect_package_manager(dst).or_else(|| detect_package_manager(src));
    let install = || {
        spec.install
            .clone()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| manager.and_then(|m| m.install_command().map(str::to_string)))
    };
    let mk = |action, reason: &str| DepsPlan {
        strategy,
        manager,
        action,
        reason: reason.to_string(),
    };
    let install_or_none = |why: &str| match install() {
        Some(command) => mk(DepsAction::Install { command }, why),
        None => mk(DepsAction::None, "nothing to install"),
    };
    match strategy {
        DepsStrategy::None => mk(DepsAction::None, "deps.strategy = none"),
        DepsStrategy::Install => install_or_none("deps.strategy = install"),
        DepsStrategy::Clone | DepsStrategy::Auto => {
            let auto = strategy == DepsStrategy::Auto;
            let Some(m) = manager.filter(|m| m.clones_node_modules()) else {
                return install_or_none("no cloneable dependency directory for this project");
            };
            let dirs = node_module_dirs(src);
            if dirs.is_empty() {
                return install_or_none("the source checkout has no node_modules");
            }
            if auto {
                let (a, b) = (lock_hash(src, m), lock_hash(dst, m));
                if a.is_none() || a != b {
                    return install_or_none("lockfile differs from the source checkout");
                }
                let probe = m
                    .lockfiles()
                    .iter()
                    .map(|f| src.join(f))
                    .find(|p| p.is_file());
                if !probe.is_some_and(|p| cow_available(&p, dst)) {
                    return install_or_none("no copy-on-write filesystem");
                }
            }
            mk(
                DepsAction::Clone { dirs },
                if auto {
                    "lockfile identical and copy-on-write available"
                } else {
                    "deps.strategy = clone"
                },
            )
        }
    }
}

/// Carry out a [`DepsAction::Clone`]: clone each directory from `src` into
/// `dst`. Other actions do nothing here (installs run as a setup step).
pub fn run_deps_clone(plan: &DepsPlan, src: &Path, dst: &Path) -> Vec<CopyResult> {
    let DepsAction::Clone { dirs } = &plan.action else {
        return vec![];
    };
    crate::files::materialize_files(
        src,
        dst,
        &crate::taskfile::FilesSpec {
            clone: dirs.clone(),
            ignore_missing: Some(true),
            ..Default::default()
        },
    )
    .into_iter()
    .filter(|r| !matches!(r.outcome, CopyOutcome::DestExists))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(p: &Path, rel: &str, c: &str) {
        let f = p.join(rel);
        fs::create_dir_all(f.parent().unwrap()).unwrap();
        fs::write(f, c).unwrap();
    }

    #[test]
    fn detects_managers() {
        let t = tempfile::tempdir().unwrap();
        assert_eq!(detect_package_manager(t.path()), None);
        w(t.path(), "package.json", "{}");
        assert_eq!(detect_package_manager(t.path()), Some(PackageManager::Npm));
        w(t.path(), "yarn.lock", "");
        assert_eq!(detect_package_manager(t.path()), Some(PackageManager::Yarn));
        w(t.path(), "pnpm-lock.yaml", "");
        assert_eq!(detect_package_manager(t.path()), Some(PackageManager::Pnpm));
        assert!(PackageManager::Cargo.install_command().is_none());
        assert!(
            PackageManager::Pnpm
                .install_command()
                .unwrap()
                .contains("--frozen-lockfile")
        );
    }

    fn pair() -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let (s, d) = (t.path().join("src"), t.path().join("dst"));
        for r in [&s, &d] {
            w(r, "package.json", "{}");
            w(r, "pnpm-lock.yaml", "lock-v1");
        }
        w(&s, "node_modules/a/index.js", "1");
        w(&s, "apps/web/node_modules/b/index.js", "2");
        w(&s, "node_modules/a/node_modules/n/x", "nested");
        (t, s, d)
    }

    #[test]
    fn finds_workspace_node_modules_not_nested() {
        let (_t, s, _d) = pair();
        assert_eq!(
            node_module_dirs(&s),
            vec!["apps/web/node_modules", "node_modules"]
        );
    }

    #[test]
    fn auto_installs_when_lockfile_differs() {
        let (_t, s, d) = pair();
        w(&d, "pnpm-lock.yaml", "lock-v2");
        let p = plan_deps(&DepsSpec::default(), &s, &d);
        assert!(matches!(p.action, DepsAction::Install { .. }), "{p:?}");
        assert!(p.reason.contains("lockfile"));
    }

    #[test]
    fn auto_clones_when_identical_if_cow_else_installs() {
        let (_t, s, d) = pair();
        let p = plan_deps(&DepsSpec::default(), &s, &d);
        match p.action {
            DepsAction::Clone { .. } => {
                let res = run_deps_clone(&p, &s, &d);
                assert_eq!(res.len(), 2);
                assert!(d.join("apps/web/node_modules/b/index.js").is_file());
            }
            DepsAction::Install { .. } => assert!(p.reason.contains("copy-on-write")),
            DepsAction::None => panic!("{p:?}"),
        }
    }

    #[test]
    fn explicit_strategies() {
        let (_t, s, d) = pair();
        let sp = |st, i: Option<&str>| DepsSpec {
            strategy: Some(st),
            install: i.map(str::to_string),
        };
        assert_eq!(
            plan_deps(&sp(DepsStrategy::None, None), &s, &d).action,
            DepsAction::None
        );
        assert_eq!(
            plan_deps(&sp(DepsStrategy::Install, Some("make deps")), &s, &d).install_command(),
            Some("make deps")
        );
        let c = plan_deps(&sp(DepsStrategy::Clone, None), &s, &d);
        assert!(matches!(c.action, DepsAction::Clone { .. }));
        let r = run_deps_clone(&c, &s, &d);
        assert!(
            r.iter()
                .all(|x| matches!(x.outcome, CopyOutcome::Cloned(_)))
        );
        // Never overwrites a second time.
        assert!(run_deps_clone(&c, &s, &d).is_empty());
        // No node_modules in the source: clone falls back to install.
        let t2 = tempfile::tempdir().unwrap();
        w(t2.path(), "package.json", "{}");
        let f = plan_deps(&sp(DepsStrategy::Clone, None), t2.path(), t2.path());
        assert!(matches!(f.action, DepsAction::Install { .. }));
    }
}
