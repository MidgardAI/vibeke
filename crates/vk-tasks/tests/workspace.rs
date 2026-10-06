//! Task workspace features (05 §4 reconcile, §5 files/deps, §6 pool health,
//! §7 setup steps). Everything lives in temp dirs: repos, worktrees and the
//! lease state.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;
use vk_tasks::*;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn cfg(root: WorktreeRoot) -> WorktreeConfig {
    WorktreeConfig {
        root,
        fetch_before_create: false,
        user: Some("demo".into()),
        ..Default::default()
    }
}

fn req(repo: &Path, title: &str) -> CreateRequest {
    CreateRequest {
        repo: repo.to_path_buf(),
        title: title.into(),
        ..Default::default()
    }
}

fn spec(copy: &[&str], link: &[&str], clone: &[&str]) -> FilesSpec {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect();
    FilesSpec {
        copy: v(copy),
        link: v(link),
        clone: v(clone),
        ignore_missing: None,
    }
}

fn assert_in(tmp: &TempDir, p: &Path) {
    let t = tmp.path().canonicalize().unwrap();
    assert!(
        p.starts_with(&t),
        "{} escaped the temp dir {}",
        p.display(),
        t.display()
    );
}

fn repo_with_worktree() -> (TempDir, PathBuf, Checkout) {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo = tmp_path.join("myrepo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    fs::write(repo.join(".gitignore"), ".env*\nnode_modules\ndata\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let co = create_worktree(
        &req(&repo, "feature"),
        &cfg(WorktreeRoot::Dir(tmp_path.join("wts"))),
    )
    .unwrap();
    assert_in(&tmp, &co.path);
    (tmp, repo, co)
}

#[test]
fn materialize_copy_link_clone_with_globs() {
    let (tmp, repo, co) = repo_with_worktree();
    fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    fs::set_permissions(repo.join(".env"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir_all(repo.join("data/fixtures")).unwrap();
    fs::write(repo.join("data/fixtures/big.bin"), "big").unwrap();
    fs::create_dir_all(repo.join("node_modules/dep")).unwrap();
    fs::write(repo.join("node_modules/dep/i.js"), "1").unwrap();
    fs::create_dir_all(repo.join("apps/web/node_modules/x")).unwrap();
    fs::write(repo.join("apps/web/node_modules/x/i.js"), "2").unwrap();
    fs::create_dir_all(repo.join("apps/api/node_modules/y")).unwrap();
    fs::write(repo.join("apps/api/node_modules/y/i.js"), "3").unwrap();

    let s = FilesSpec {
        ignore_missing: Some(true),
        ..spec(
            &[".env", ".env.local"],
            &["data/fixtures"],
            &["node_modules", "apps/*/node_modules"],
        )
    };
    let res = materialize_files(&repo, &co.path, &s);
    let find = |rel: &str| {
        res.iter()
            .find(|r| r.rel == rel)
            .unwrap_or_else(|| panic!("{rel}: {res:?}"))
    };

    assert_eq!(find(".env").outcome, CopyOutcome::Copied);
    assert!(
        find(".env").hash.is_some(),
        "copy reports a hash, never contents"
    );
    assert_eq!(find(".env.local").outcome, CopyOutcome::MissingSource);
    // Copies are real files (a secret is never a symlink) and keep their mode.
    let meta = fs::symlink_metadata(co.path.join(".env")).unwrap();
    assert!(meta.file_type().is_file());
    assert_eq!(meta.permissions().mode() & 0o777, 0o600);

    assert_eq!(find("data/fixtures").outcome, CopyOutcome::Linked);
    assert_eq!(
        fs::read_link(co.path.join("data/fixtures")).unwrap(),
        repo.join("data/fixtures")
    );

    assert!(matches!(
        find("node_modules").outcome,
        CopyOutcome::Cloned(_)
    ));
    assert!(matches!(
        find("apps/web/node_modules").outcome,
        CopyOutcome::Cloned(_)
    ));
    assert!(matches!(
        find("apps/api/node_modules").outcome,
        CopyOutcome::Cloned(_)
    ));
    assert_eq!(
        fs::read_to_string(co.path.join("apps/api/node_modules/y/i.js")).unwrap(),
        "3"
    );
    // The clone is independent of the source.
    fs::write(co.path.join("node_modules/dep/i.js"), "changed").unwrap();
    assert_eq!(
        fs::read_to_string(repo.join("node_modules/dep/i.js")).unwrap(),
        "1"
    );

    // Re-running never overwrites and never fails.
    let again = materialize_files(&repo, &co.path, &s);
    assert_eq!(
        again.iter().find(|r| r.rel == ".env").unwrap().outcome,
        CopyOutcome::DestExists
    );
    assert_eq!(
        fs::read_to_string(co.path.join("node_modules/dep/i.js")).unwrap(),
        "changed"
    );
    assert_in(&tmp, &co.path);
}

#[test]
fn ignore_missing_false_reports_failures_and_paths_cannot_escape() {
    let (_tmp, repo, co) = repo_with_worktree();
    let s = FilesSpec {
        ignore_missing: Some(false),
        ..spec(&["nope.env"], &["../outside"], &["/etc"])
    };
    let res = materialize_files(&repo, &co.path, &s);
    assert!(matches!(res[0].outcome, CopyOutcome::Failed(_)), "{res:?}");
    assert!(
        matches!(res[1].outcome, CopyOutcome::Rejected(_)),
        "{res:?}"
    );
    assert!(
        matches!(res[2].outcome, CopyOutcome::Rejected(_)),
        "{res:?}"
    );
    assert!(!co.path.join("../outside").exists());
}

#[test]
fn deps_clone_strategy_end_to_end() {
    let (_tmp, repo, co) = repo_with_worktree();
    fs::write(repo.join("package.json"), "{}").unwrap();
    fs::write(repo.join("pnpm-lock.yaml"), "lock").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "pkg"]);
    // The worktree was created before the commit: give it the same lockfiles.
    fs::write(co.path.join("package.json"), "{}").unwrap();
    fs::write(co.path.join("pnpm-lock.yaml"), "lock").unwrap();
    fs::create_dir_all(repo.join("node_modules/p")).unwrap();
    fs::write(repo.join("node_modules/p/i.js"), "1").unwrap();

    let plan = plan_deps(&DepsSpec::default(), &repo, &co.path);
    assert_eq!(plan.manager, Some(PackageManager::Pnpm));
    match &plan.action {
        DepsAction::Clone { .. } => {
            let r = run_deps_clone(&plan, &repo, &co.path);
            assert!(
                r.iter()
                    .all(|x| matches!(x.outcome, CopyOutcome::Cloned(_)))
            );
            assert!(co.path.join("node_modules/p/i.js").is_file());
        }
        // Filesystems without reflinks install instead.
        DepsAction::Install { command } => {
            assert!(command.starts_with("pnpm install --frozen-lockfile"));
            assert!(plan.reason.contains("copy-on-write"));
        }
        DepsAction::None => panic!("{plan:?}"),
    }
    // A changed lockfile always installs.
    fs::write(co.path.join("pnpm-lock.yaml"), "lock2").unwrap();
    let plan = plan_deps(&DepsSpec::default(), &repo, &co.path);
    assert!(matches!(plan.action, DepsAction::Install { .. }));
}

#[test]
fn reconcile_reports_missing_moved_and_orphans_and_deletes_nothing() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo = tmp_path.join("myrepo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let root = WorktreeRoot::Dir(tmp_path.join("wts"));
    let c = cfg(root.clone());

    let kept = create_worktree(&req(&repo, "kept"), &c).unwrap();
    let gone = create_worktree(&req(&repo, "gone"), &c).unwrap();
    let moved = create_worktree(&req(&repo, "moved"), &c).unwrap();
    let orphan = create_worktree(&req(&repo, "orphan"), &c).unwrap();
    for co in [&kept, &gone, &moved, &orphan] {
        assert_in(&tmp, &co.path);
    }
    // Leftover directory that is not a worktree.
    let stray = tmp_path.join("wts/myrepo/leftover");
    fs::create_dir_all(&stray).unwrap();
    fs::write(stray.join("f"), "x").unwrap();
    // `.trash` is the reaper's, never reported.
    fs::create_dir_all(tmp_path.join("wts/myrepo/.trash/x")).unwrap();

    // Removed outside Vibeke.
    fs::remove_dir_all(&gone.path).unwrap();
    // Branch switched inside the worktree.
    git(&moved.path, &["switch", "-q", "-c", "elsewhere"]);

    let tracked = |co: &Checkout, id: &str| TrackedCheckout {
        task_id: id.into(),
        path: co.path.clone(),
        branch: co.branch.clone(),
    };
    let tracked = vec![
        tracked(&kept, "k1"),
        tracked(&gone, "k2"),
        tracked(&moved, "k3"),
    ];
    let before: Vec<_> = list_worktrees(&repo)
        .unwrap()
        .iter()
        .map(|w| w.path.clone())
        .collect();
    let rep = reconcile(&repo, &root, &tracked).unwrap();

    assert_eq!(rep.missing.len(), 1, "{rep:?}");
    assert_eq!(rep.missing[0].task_id, "k2");
    assert_eq!(rep.missing[0].reason, MissingReason::Prunable);
    assert_eq!(rep.branch_moved.len(), 1);
    assert_eq!(rep.branch_moved[0].task_id, "k3");
    assert_eq!(rep.branch_moved[0].actual.as_deref(), Some("elsewhere"));
    let orphan_paths: Vec<_> = rep
        .orphans
        .iter()
        .map(|o| (o.path.clone(), o.kind))
        .collect();
    assert!(orphan_paths.contains(&(orphan.path.clone(), OrphanKind::Worktree)));
    assert!(orphan_paths.contains(&(stray.clone(), OrphanKind::Directory)));
    assert_eq!(rep.orphans.len(), 2, "{rep:?}");
    assert!(!rep.is_clean());

    // Nothing was deleted or pruned: every worktree, directory and branch is still there.
    let after: Vec<_> = list_worktrees(&repo)
        .unwrap()
        .iter()
        .map(|w| w.path.clone())
        .collect();
    assert_eq!(before, after);
    assert!(orphan.path.is_dir() && stray.join("f").is_file() && kept.path.is_dir());
    let gb = gone.branch.clone().unwrap();
    assert!(git(&repo, &["branch", "--list", &gb]).contains(&gb));

    // A clean state reports clean.
    let clean = reconcile(&repo, &root, &[tracked_of(&kept)]).unwrap();
    assert!(clean.missing.is_empty() && clean.branch_moved.is_empty());
}

fn tracked_of(co: &Checkout) -> TrackedCheckout {
    TrackedCheckout {
        task_id: "x".into(),
        path: co.path.clone(),
        branch: co.branch.clone(),
    }
}

#[test]
fn reconcile_sibling_root_finds_orphans() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().canonicalize().unwrap();
    let repo = tmp_path.join("proj");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a"), "a").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "i"]);
    let c = cfg(WorktreeRoot::Sibling);
    let co = create_worktree(&req(&repo, "side"), &c).unwrap();
    assert_in(&tmp, &co.path);
    // A worktree elsewhere is not in the task area and is not an orphan.
    let other = tmp_path.join("elsewhere");
    git(
        &repo,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "manual",
            other.to_str().unwrap(),
        ],
    );
    let rep = reconcile(&repo, &WorktreeRoot::Sibling, &[]).unwrap();
    assert_eq!(rep.orphans.len(), 1, "{rep:?}");
    assert_eq!(rep.orphans[0].path, co.path);
}

#[test]
fn pool_health_flags_exhaustion_overlap_and_stray_leases() {
    let lease = |s: u16, e: u16, id: &str| Lease {
        start: s,
        end: e,
        task_id: id.into(),
        session: "s".into(),
        owner_pid: None,
        created_at: 0,
    };
    let pool = PortPool::parse("20000-20029", 10).unwrap();
    // Healthy: nothing leased, outside the ephemeral range.
    let h = pool_health(pool, &[], Some((32768, 60999)));
    assert_eq!((h.capacity, h.free, h.leased), (3, 3, 0));
    assert!(h.warnings.is_empty(), "{:?}", h.warnings);
    // Exhausted.
    let full = [
        lease(20000, 20009, "a"),
        lease(20010, 20019, "b"),
        lease(20020, 20029, "c"),
    ];
    let h = pool_health(pool, &full, None);
    assert_eq!(h.free, 0);
    assert!(
        h.warnings.iter().any(|w| w.contains("exhausted")),
        "{:?}",
        h.warnings
    );
    // Overlapping the ephemeral range.
    let h = pool_health(pool, &[], Some((20010, 65535)));
    assert!(h.warnings.iter().any(|w| w.contains("ephemeral")));
    // A lease outside the pool and two overlapping leases.
    let odd = [
        lease(30000, 30009, "z"),
        lease(20000, 20009, "a"),
        lease(20005, 20014, "b"),
    ];
    let h = pool_health(pool, &odd, None);
    assert!(h.warnings.iter().any(|w| w.contains("outside")));
    assert!(h.warnings.iter().any(|w| w.contains("overlap")));
    // Live table: lease through the real allocator and read it back.
    let tmp = tempfile::tempdir().unwrap();
    let pl = PortLeases::new(tmp.path(), pool).with_probe(false);
    pl.lease(&LeaseRequest::new("t1", "s")).unwrap();
    let h = pl.health().unwrap();
    assert_eq!((h.capacity, h.free, h.leased), (3, 2, 1));
}

#[test]
fn lease_sized_honours_ports_count_and_alignment() {
    let tmp = tempfile::tempdir().unwrap();
    let pool = PortPool::parse("20000-20099", 10).unwrap();
    let pl = PortLeases::new(tmp.path(), pool).with_probe(false);
    let a = pl.lease_sized(&LeaseRequest::new("a", "s"), 4).unwrap();
    assert_eq!((a.start, a.end), (20000, 20003));
    let b = pl.lease_sized(&LeaseRequest::new("b", "s"), 25).unwrap();
    assert_eq!((b.start, b.end), (20025, 20049));
    // A default-size lease never overlaps either.
    let c = pl.lease(&LeaseRequest::new("c", "s")).unwrap();
    assert!(
        c.start > b.end || c.end < a.start || (c.start > a.end && c.end < b.start),
        "{c:?}"
    );
    assert!(
        pl.health()
            .unwrap()
            .warnings
            .iter()
            .all(|w| !w.contains("overlap"))
    );
}

#[test]
fn setup_runs_install_then_run_commands_then_script_and_stops_on_failure() {
    let (_tmp, _repo, co) = repo_with_worktree();
    fs::create_dir_all(co.path.join(".vibeke")).unwrap();
    fs::write(
        co.path.join(".vibeke/setup.sh"),
        "echo script >> order.txt\n",
    )
    .unwrap();
    let log = co.path.join(".vibeke/setup.log");
    let mut o = SetupOptions {
        worktree: co.path.clone(),
        script: ".vibeke/setup.sh".into(),
        commands: vec![
            "echo install >> order.txt".into(),
            "echo \"db=$DATABASE_URL port=$PORT\" >> order.txt".into(),
        ],
        task_id: "k7".into(),
        lease: None,
        extra_env: vec![("DATABASE_URL".into(), "postgres://x/app_feature".into())],
        log_path: log.clone(),
        timeout: Some(Duration::from_secs(20)),
    };
    let out = run_setup(&o, &CancelToken::new()).unwrap();
    assert_eq!(out.status, SetupStatus::Succeeded);
    let order = fs::read_to_string(co.path.join("order.txt")).unwrap();
    assert_eq!(
        order.lines().collect::<Vec<_>>(),
        ["install", "db=postgres://x/app_feature port=", "script"]
    );
    assert!(fs::read_to_string(&log).unwrap().contains("$ echo install"));

    // A failing step ends setup before the later ones.
    fs::remove_file(co.path.join("order.txt")).unwrap();
    o.commands = vec!["exit 3".into(), "echo after >> order.txt".into()];
    let out = run_setup(&o, &CancelToken::new()).unwrap();
    assert_eq!(out.status, SetupStatus::Failed { exit_code: Some(3) });
    assert!(!co.path.join("order.txt").exists());

    // Nothing to run is Skipped.
    o.commands.clear();
    o.script = PathBuf::new();
    assert_eq!(
        run_setup(&o, &CancelToken::new()).unwrap().status,
        SetupStatus::Skipped
    );
}
