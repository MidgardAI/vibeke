use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
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
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Temp dir holding `myrepo/` with one commit on `main`.
fn fixture() -> (TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path().join("myrepo");
    fs::create_dir(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a.txt"), "a\n").unwrap();
    fs::write(repo.join(".gitignore"), ".env\n.env.local\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    let repo = repo.canonicalize().unwrap();
    (tmp, repo)
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

#[test]
fn slugs() {
    assert_eq!(slugify("Fix login redirect!", 40), "fix-login-redirect");
    assert_eq!(slugify("Rør på Ærlig Åse", 40), "ror-pa-aerlig-ase");
    assert_eq!(slugify("  ---  ", 40), "task");
    let s = slugify("a very long title that goes on and on and on forever", 20);
    assert!(s.len() <= 20 && !s.ends_with('-'), "{s}");
    let taken: HashSet<&str> = ["x", "x-2"].into();
    assert_eq!(unique_slug("x", |c| taken.contains(c)), "x-3");
    assert_eq!(render_branch("{user}/{slug}", "demo", "fix"), "demo/fix");
}

#[test]
fn detect_repo_and_linked_worktree() {
    let (tmp, repo) = fixture();
    let info = repo_root(&repo).unwrap();
    assert_eq!(info.root, repo);
    assert_eq!(info.vcs, Vcs::Git);
    assert_eq!(info.default_branch.as_deref(), Some("main"));
    assert_eq!(info.current_branch.as_deref(), Some("main"));
    assert!(!info.is_linked_worktree);
    assert!(info.remote_url.is_none());

    let wt_root = tmp.path().join("wts");
    let co = create_worktree(&req(&repo, "linked"), &cfg(WorktreeRoot::Dir(wt_root))).unwrap();
    let linfo = repo_root(&co.path).unwrap();
    assert!(linfo.is_linked_worktree);
    assert_eq!(linfo.root, repo);
    assert_eq!(linfo.worktree_root, co.path);
    assert_eq!(linfo.current_branch.as_deref(), Some("demo/linked"));

    let none = tmp.path().join("plain");
    fs::create_dir(&none).unwrap();
    assert!(repo_root(&none).is_none());
    assert_eq!(detect(&none).vcs, Vcs::None);
}

#[test]
fn create_in_dir_root_dedupes_and_lists() {
    let (tmp, repo) = fixture();
    let root = tmp.path().join("wts");
    let c = cfg(WorktreeRoot::Dir(root.clone()));
    let a = create_worktree(&req(&repo, "Fix Login"), &c).unwrap();
    let b = create_worktree(&req(&repo, "Fix Login"), &c).unwrap();
    let root = root.canonicalize().unwrap();
    assert_eq!(a.path, root.join("myrepo/fix-login"));
    assert_eq!(a.branch.as_deref(), Some("demo/fix-login"));
    assert!(a.created_branch);
    assert_eq!(a.base_ref.as_deref(), Some("main"));
    assert_eq!(b.path, root.join("myrepo/fix-login-2"));
    assert_eq!(b.branch.as_deref(), Some("demo/fix-login-2"));
    assert!(a.path.join("a.txt").exists());

    let list = list_worktrees(&repo).unwrap();
    assert_eq!(list.len(), 3);
    assert!(list[0].is_main && list[0].path == repo);
    assert_eq!(list[0].branch.as_deref(), Some("main"));
    let la = list.iter().find(|w| w.path == a.path).unwrap();
    assert_eq!(la.branch.as_deref(), Some("demo/fix-login"));
    assert!(!la.locked && !la.prunable && la.head.is_some());

    // open existing
    let o = open_worktree(&repo, &a.path).unwrap();
    assert_eq!(o.branch, a.branch);
    assert!(!o.created_branch);

    // lock + prunable reporting
    git(
        &repo,
        &[
            "worktree",
            "lock",
            "--reason",
            "busy",
            a.path.to_str().unwrap(),
        ],
    );
    let la = find_worktree(&repo, &a.path).unwrap();
    assert!(la.locked);
    assert_eq!(la.lock_reason.as_deref(), Some("busy"));
    git(&repo, &["worktree", "unlock", a.path.to_str().unwrap()]);
    fs::remove_dir_all(&b.path).unwrap();
    assert!(find_worktree(&repo, &b.path).unwrap().prunable);
    assert!(open_worktree(&repo, &b.path).is_err());
}

#[test]
fn create_sibling_root_and_explicit_branch() {
    let (tmp, repo) = fixture();
    let c = cfg(WorktreeRoot::Sibling);
    let co = create_worktree(&req(&repo, "todo thing"), &c).unwrap();
    assert_eq!(
        co.path,
        tmp.path().canonicalize().unwrap().join("myrepo-todo-thing")
    );

    // explicit new branch used as-is
    let r = CreateRequest {
        branch: Some("feature/x".into()),
        ..req(&repo, "other")
    };
    let co2 = create_worktree(&r, &c).unwrap();
    assert_eq!(co2.branch.as_deref(), Some("feature/x"));

    // existing branch held by another worktree is refused
    let r = CreateRequest {
        branch: Some("feature/x".into()),
        ..req(&repo, "third")
    };
    assert!(matches!(
        create_worktree(&r, &c),
        Err(Error::BranchInUse { .. })
    ));

    // existing free branch gets checked out
    git(&repo, &["branch", "free-branch"]);
    let r = CreateRequest {
        branch: Some("free-branch".into()),
        ..req(&repo, "fourth")
    };
    let co3 = create_worktree(&r, &c).unwrap();
    assert!(!co3.created_branch);
    assert_eq!(co3.branch.as_deref(), Some("free-branch"));

    // branch name collision gets deduped when derived from template
    git(&repo, &["branch", "demo/dup"]);
    let co4 = create_worktree(&req(&repo, "dup"), &c).unwrap();
    assert_eq!(co4.branch.as_deref(), Some("demo/dup-2"));
}

#[test]
fn fetch_before_create_uses_origin_default() {
    let (tmp, repo) = fixture();
    let origin = tmp.path().join("origin.git");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            "--bare",
            repo.to_str().unwrap(),
            origin.to_str().unwrap(),
        ],
    );
    git(
        &repo,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&repo, &["fetch", "-q", "origin"]);
    git(&repo, &["remote", "set-head", "origin", "main"]);
    // Advance origin/main via a second clone.
    let other = tmp.path().join("other");
    git(
        tmp.path(),
        &[
            "clone",
            "-q",
            origin.to_str().unwrap(),
            other.to_str().unwrap(),
        ],
    );
    fs::write(other.join("new.txt"), "n").unwrap();
    git(&other, &["add", "."]);
    git(&other, &["commit", "-q", "-m", "more"]);
    git(&other, &["push", "-q", "origin", "main"]);

    let info = repo_root(&repo).unwrap();
    assert_eq!(info.remote_url.as_deref(), origin.to_str());
    assert_eq!(info.default_branch.as_deref(), Some("main"));

    let mut c = cfg(WorktreeRoot::Dir(tmp.path().join("wts")));
    c.fetch_before_create = true;
    let co = create_worktree(&req(&repo, "fresh"), &c).unwrap();
    assert_eq!(co.fetch, FetchOutcome::Fetched);
    assert_eq!(co.base_ref.as_deref(), Some("origin/main"));
    assert!(
        co.path.join("new.txt").exists(),
        "based on fetched origin/main"
    );
    let st = branch_status(&co.path, Some("origin/main")).unwrap();
    assert!(st.upstream.is_none(), "--no-track");
    assert_eq!((st.ahead, st.behind), (0, 0));
    assert_eq!(st.compared_to.as_deref(), Some("origin/main"));
}

#[test]
fn copy_untracked_files() {
    let (tmp, repo) = fixture();
    fs::write(repo.join(".env"), "SECRET=1\n").unwrap();
    fs::set_permissions(repo.join(".env"), fs::Permissions::from_mode(0o600)).unwrap();
    fs::create_dir_all(repo.join("apps/web")).unwrap();
    fs::write(repo.join("apps/web/.env.local"), "X=1\n").unwrap();
    let co = create_worktree(
        &req(&repo, "env"),
        &cfg(WorktreeRoot::Dir(tmp.path().join("wts"))),
    )
    .unwrap();
    // Pre-existing destination must survive.
    fs::write(co.path.join(".env.local"), "mine\n").unwrap();
    fs::write(repo.join(".env.local"), "theirs\n").unwrap();

    let files: Vec<String> = [
        ".env",
        ".env.local",
        "apps/web/.env.local",
        "missing.env",
        "../escape",
        "/etc/passwd",
        "apps",
    ]
    .map(String::from)
    .to_vec();
    let res = copy_files(&repo, &co.path, &files).unwrap();
    let get = |n: &str| res.iter().find(|r| r.rel == n).unwrap().outcome.clone();
    assert_eq!(get(".env"), CopyOutcome::Copied);
    assert_eq!(get(".env.local"), CopyOutcome::DestExists);
    assert_eq!(get("apps/web/.env.local"), CopyOutcome::Copied);
    assert_eq!(get("missing.env"), CopyOutcome::MissingSource);
    assert!(matches!(get("../escape"), CopyOutcome::Rejected(_)));
    assert!(matches!(get("/etc/passwd"), CopyOutcome::Rejected(_)));
    assert!(matches!(get("apps"), CopyOutcome::Rejected(_)));
    assert_eq!(
        fs::read_to_string(co.path.join(".env")).unwrap(),
        "SECRET=1\n"
    );
    assert_eq!(
        fs::metadata(co.path.join(".env"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::read_to_string(co.path.join(".env.local")).unwrap(),
        "mine\n"
    );
    assert!(co.path.join("apps/web/.env.local").exists());
    assert_eq!(default_copy_files(), vec![".env", ".env.local"]);
}

fn setup_opts(wt: &Path, log: &Path, lease: Option<Lease>) -> SetupOptions {
    SetupOptions {
        worktree: wt.to_path_buf(),
        script: ".vibeke/setup.sh".into(),
        task_id: "k7".into(),
        lease,
        extra_env: vec![("EXTRA".into(), "yes".into())],
        log_path: log.to_path_buf(),
        timeout: Some(Duration::from_secs(20)),
    }
}

#[test]
fn setup_script_env_and_log() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join("wt");
    fs::create_dir_all(wt.join(".vibeke")).unwrap();
    fs::write(
        wt.join(".vibeke/setup.sh"),
        "echo task=$VIBEKE_TASK_ID base=$VIBEKE_PORT_BASE count=$VIBEKE_PORT_COUNT p0=$VIBEKE_PORT_0 p9=$VIBEKE_PORT_9 port=$PORT extra=$EXTRA\necho oops >&2\npwd\n",
    )
    .unwrap();
    let lease = Lease {
        start: 20010,
        end: 20019,
        task_id: "k7".into(),
        session: "s".into(),
        owner_pid: None,
        created_at: 0,
    };
    let log = tmp.path().join("logs/setup.log");
    let out = run_setup(&setup_opts(&wt, &log, Some(lease)), &CancelToken::new()).unwrap();
    assert_eq!(out.status, SetupStatus::Succeeded);
    let text = fs::read_to_string(&log).unwrap();
    assert!(
        text.contains("task=k7 base=20010 count=10 p0=20010 p9=20019 port=20010 extra=yes"),
        "{text}"
    );
    assert!(text.contains("oops"), "stderr combined: {text}");
    assert!(text.contains("wt"), "cwd is worktree: {text}");

    fs::write(wt.join(".vibeke/setup.sh"), "exit 3\n").unwrap();
    let out = run_setup(&setup_opts(&wt, &log, None), &CancelToken::new()).unwrap();
    assert_eq!(out.status, SetupStatus::Failed { exit_code: Some(3) });

    fs::remove_file(wt.join(".vibeke/setup.sh")).unwrap();
    let out = run_setup(&setup_opts(&wt, &log, None), &CancelToken::new()).unwrap();
    assert_eq!(out.status, SetupStatus::Skipped);
}

#[test]
fn setup_timeout_and_cancel() {
    let tmp = tempfile::tempdir().unwrap();
    let wt = tmp.path().join("wt");
    fs::create_dir_all(wt.join(".vibeke")).unwrap();
    fs::write(wt.join(".vibeke/setup.sh"), "sleep 30 &\nsleep 30\n").unwrap();
    let log = tmp.path().join("s.log");

    let mut o = setup_opts(&wt, &log, None);
    o.timeout = Some(Duration::from_millis(200));
    let t = std::time::Instant::now();
    let out = run_setup(&o, &CancelToken::new()).unwrap();
    assert_eq!(out.status, SetupStatus::TimedOut);
    assert!(t.elapsed() < Duration::from_secs(10));

    let h = spawn_setup(setup_opts(&wt, &log, None));
    std::thread::sleep(Duration::from_millis(200));
    assert!(!h.is_finished());
    h.cancel();
    let out = h.wait().unwrap();
    assert_eq!(out.status, SetupStatus::Cancelled);
}

fn pool() -> PortPool {
    PortPool::parse("20000-29999", 10).unwrap()
}

#[test]
fn pool_parsing() {
    assert_eq!(
        PortPool::parse("20000-29999", 10).unwrap(),
        PortPool::default()
    );
    assert!(PortPool::parse("nope", 10).is_err());
    assert!(PortPool::parse("5-1", 10).is_err());
}

#[test]
fn leases_basic_idempotent_release() {
    let tmp = tempfile::tempdir().unwrap();
    let pl = PortLeases::new(tmp.path().join("machine"), pool());
    let a = pl.lease(&LeaseRequest::new("t1", "s1")).unwrap();
    let b = pl.lease(&LeaseRequest::new("t2", "s2")).unwrap();
    assert_eq!(a.start % 10, 0);
    assert_eq!(a.count(), 10);
    assert_eq!(a.port(3), Some(a.start + 3));
    assert_eq!(a.port(10), None);
    assert!(a.end < b.start || b.end < a.start);
    assert_eq!(pl.lease(&LeaseRequest::new("t1", "s1")).unwrap(), a);
    assert_eq!(pl.list().unwrap().len(), 2);
    assert!(pl.release("t1").unwrap());
    assert!(!pl.release("t1").unwrap());
    // freed block gets reused
    let c = pl.lease(&LeaseRequest::new("t3", "s1")).unwrap();
    assert_eq!(c.start, a.start);
    assert_eq!(pl.lease_for("t2").unwrap().unwrap(), b);
}

#[test]
fn leases_exhaustion_and_busy_port_skip() {
    let tmp = tempfile::tempdir().unwrap();
    let small = PortPool::parse("40000-40019", 10).unwrap();
    let pl = PortLeases::new(tmp.path(), small);
    let a = pl.lease(&LeaseRequest::new("a", "s")).unwrap();
    let _b = pl.lease(&LeaseRequest::new("b", "s")).unwrap();
    assert!(matches!(
        pl.lease(&LeaseRequest::new("c", "s")),
        Err(Error::PortsExhausted)
    ));
    pl.release("a").unwrap();

    // Occupy a port inside the free block: it must be skipped.
    let busy = std::net::TcpListener::bind(("127.0.0.1", a.start + 4));
    if busy.is_ok() {
        assert!(matches!(
            pl.lease(&LeaseRequest::new("c", "s")),
            Err(Error::PortsExhausted)
        ));
        drop(busy);
        assert_eq!(
            pl.lease(&LeaseRequest::new("c", "s")).unwrap().start,
            a.start
        );
    }
}

#[test]
fn leases_threads_never_overlap() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = Arc::new(tmp.path().join("m"));
    let handles: Vec<_> = (0..8)
        .map(|t| {
            let dir = dir.clone();
            std::thread::spawn(move || {
                // Separate handles ~ separate sessions.
                let pl = PortLeases::new(dir.as_path(), pool()).with_probe(false);
                (0..10)
                    .map(|i| {
                        pl.lease(&LeaseRequest::new(format!("t{t}-{i}"), format!("s{t}")))
                            .unwrap()
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    let all: Vec<Lease> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    assert_eq!(all.len(), 80);
    let starts: HashSet<u16> = all.iter().map(|l| l.start).collect();
    assert_eq!(starts.len(), 80, "overlapping blocks handed out");
    let pl = PortLeases::new(dir.as_path(), pool());
    assert_eq!(pl.list().unwrap().len(), 80);
}

/// Child-process worker: only does work when VK_PORT_WORKER_DIR is set.
#[test]
fn port_worker_child() {
    let Ok(dir) = std::env::var("VK_PORT_WORKER_DIR") else {
        return;
    };
    let id = std::env::var("VK_PORT_WORKER_ID").unwrap();
    let pl = PortLeases::new(dir, pool()).with_probe(false);
    for i in 0..10 {
        let l = pl
            .lease(&LeaseRequest {
                owner_pid: None,
                ..LeaseRequest::new(format!("w{id}-{i}"), "child")
            })
            .unwrap();
        println!("LEASED {}", l.start);
    }
}

#[test]
fn leases_cross_process_exclusion() {
    let tmp = tempfile::tempdir().unwrap();
    let exe = std::env::current_exe().unwrap();
    let children: Vec<_> = (0..4)
        .map(|i| {
            Command::new(&exe)
                .args([
                    "port_worker_child",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("VK_PORT_WORKER_DIR", tmp.path())
                .env("VK_PORT_WORKER_ID", i.to_string())
                .output()
        })
        .collect::<Vec<_>>();
    // (spawned sequentially by map; run the parallel variant below too)
    let mut starts = Vec::new();
    for c in children {
        let o = c.unwrap();
        assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(n) = l.split_once("LEASED ").map(|x| x.1) {
                starts.push(n.trim().parse::<u16>().unwrap());
            }
        }
    }
    assert_eq!(starts.len(), 40);

    // Truly parallel children, plus this process, on the same table.
    let spawned: Vec<_> = (10..14)
        .map(|i| {
            Command::new(&exe)
                .args([
                    "port_worker_child",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("VK_PORT_WORKER_DIR", tmp.path())
                .env("VK_PORT_WORKER_ID", i.to_string())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let me = PortLeases::new(tmp.path(), pool()).with_probe(false);
    for i in 0..10 {
        starts.push(
            me.lease(&LeaseRequest::new(format!("me-{i}"), "parent"))
                .unwrap()
                .start,
        );
    }
    for c in spawned {
        let o = c.wait_with_output().unwrap();
        assert!(o.status.success());
        for l in String::from_utf8_lossy(&o.stdout).lines() {
            if let Some(n) = l.split_once("LEASED ").map(|x| x.1) {
                starts.push(n.trim().parse::<u16>().unwrap());
            }
        }
    }
    assert_eq!(starts.len(), 90);
    let uniq: HashSet<_> = starts.iter().collect();
    assert_eq!(uniq.len(), 90, "duplicate block across processes");
    assert_eq!(me.list().unwrap().len(), 90);
}

#[test]
fn dead_owner_lease_expires() {
    let tmp = tempfile::tempdir().unwrap();
    let small = PortPool::parse("41000-41009", 10).unwrap();
    let pl = PortLeases::new(tmp.path(), small);

    let mut child = Command::new("sh").args(["-c", "exit 0"]).spawn().unwrap();
    let dead_pid = child.id();
    child.wait().unwrap();

    let req = LeaseRequest {
        task_id: "ghost".into(),
        session: "s".into(),
        owner_pid: Some(dead_pid),
    };
    let ghost = pl.lease(&req).unwrap();
    // Dead owner: not listed, block reusable.
    assert!(pl.list().unwrap().is_empty());
    let live = pl.lease(&LeaseRequest::new("live", "s")).unwrap();
    assert_eq!(live.start, ghost.start);

    // Ownerless (parked) leases never expire; live-owner leases stay.
    pl.set_owner("live", None).unwrap();
    assert_eq!(pl.gc().unwrap().len(), 0);
    assert_eq!(pl.list().unwrap().len(), 1);
    pl.set_owner("live", Some(dead_pid)).unwrap();
    assert_eq!(pl.gc().unwrap().len(), 1);
}

#[test]
fn branch_status_counts() {
    let (tmp, repo) = fixture();
    let co = create_worktree(
        &req(&repo, "status"),
        &cfg(WorktreeRoot::Dir(tmp.path().join("wts"))),
    )
    .unwrap();
    let s = branch_status(&co.path, Some("main")).unwrap();
    assert_eq!(s.branch.as_deref(), Some("demo/status"));
    assert_eq!((s.ahead, s.behind, s.dirty_files), (0, 0, 0));

    fs::write(co.path.join("new.txt"), "x").unwrap(); // untracked
    fs::write(co.path.join("a.txt"), "changed\n").unwrap(); // modified
    git(&co.path, &["add", "a.txt"]);
    git(&co.path, &["commit", "-q", "-m", "c1"]);
    fs::write(co.path.join("a.txt"), "changed again\n").unwrap();
    // main moves ahead
    fs::write(repo.join("m.txt"), "m").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "m1"]);

    let s = branch_status(&co.path, Some("main")).unwrap();
    assert_eq!((s.ahead, s.behind), (1, 1));
    assert_eq!(s.dirty_files, 2);
    assert_eq!(s.untracked, 1);
    assert_eq!(s.compared_to.as_deref(), Some("main"));
    let b = removal_blockers(&co.path).unwrap();
    assert_eq!((b.dirty_files, b.unpushed_commits), (2, 1));

    let ds = diff_stat(&co.path, Some("main")).unwrap();
    assert_eq!(ds.files, 1);
    assert!(is_merged(&repo, "main", "main").unwrap());
    assert!(!is_merged(&repo, "demo/status", "main").unwrap());
}

#[test]
fn async_removal_dirty_refused_then_force() {
    let (tmp, repo) = fixture();
    let root = tmp.path().join("wts");
    let c = cfg(WorktreeRoot::Dir(root.clone()));
    let co = create_worktree(&req(&repo, "rm me"), &c).unwrap();
    fs::create_dir_all(co.path.join("node_modules/x")).unwrap();
    for i in 0..50 {
        fs::write(co.path.join(format!("node_modules/x/f{i}")), "x").unwrap();
    }
    // untracked files = dirty
    let job = start_remove(&co.path, RemoveOptions::default());
    let ev: Vec<_> = job.events.iter().collect();
    assert!(matches!(ev[0], RemovalEvent::Started));
    assert!(
        matches!(ev.last().unwrap(), RemovalEvent::Refused { dirty_files, .. } if *dirty_files >= 1),
        "{ev:?}"
    );
    assert!(matches!(job.wait(), RemovalState::Refused(_)));
    assert!(co.path.exists());

    let trash = tmp.path().join("trash");
    let job = start_remove(
        &co.path,
        RemoveOptions {
            force: true,
            trash_root: Some(trash.clone()),
            delete_branch: true,
            ..Default::default()
        },
    );
    let ev: Vec<_> = job.events.iter().collect();
    assert!(
        ev.iter()
            .any(|e| matches!(e, RemovalEvent::Detached { trash: Some(_) })),
        "{ev:?}"
    );
    assert!(
        ev.iter()
            .any(|e| matches!(e, RemovalEvent::BranchDeleted(_)))
    );
    assert_eq!(ev.last().unwrap(), &RemovalEvent::Removed);
    assert_eq!(job.wait(), RemovalState::Done);
    assert!(!co.path.exists());
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
    assert_eq!(list_worktrees(&repo).unwrap().len(), 1);
    assert!(git(&repo, &["branch", "--list", "demo/rm-me"]).is_empty());
}

#[test]
fn removal_clean_and_unpushed_protection() {
    let (tmp, repo) = fixture();
    let c = cfg(WorktreeRoot::Dir(tmp.path().join("wts")));
    let clean = create_worktree(&req(&repo, "clean"), &c).unwrap();
    let job = start_remove(&clean.path, RemoveOptions::default());
    assert_eq!(job.wait(), RemovalState::Done);
    assert!(!clean.path.exists());
    assert!(
        !tmp.path()
            .join("wts/myrepo/.trash")
            .read_dir()
            .unwrap()
            .any(|_| true)
    );

    // Unpushed commit on a branch of its own: refused, archive keeps branch only with force.
    let co = create_worktree(&req(&repo, "work"), &c).unwrap();
    fs::write(co.path.join("w.txt"), "w").unwrap();
    git(&co.path, &["add", "."]);
    git(&co.path, &["commit", "-q", "-m", "w"]);
    let job = start_remove(&co.path, RemoveOptions::default());
    assert!(matches!(job.wait(), RemovalState::Refused(_)));

    let job = archive_worktree(
        &co.path,
        RemoveOptions {
            force: true,
            ..Default::default()
        },
    );
    assert_eq!(job.wait(), RemovalState::Done);
    assert!(!co.path.exists());
    assert!(!git(&repo, &["branch", "--list", "demo/work"]).is_empty());

    // restore from branch
    let back = restore_worktree(&repo, "demo/work", "work", &c).unwrap();
    assert!(back.path.join("w.txt").exists());

    // main worktree can never be removed
    let job = start_remove(
        &repo,
        RemoveOptions {
            force: true,
            ..Default::default()
        },
    );
    assert!(matches!(job.wait(), RemovalState::Failed(_)));
    assert!(repo.join("a.txt").exists());
}

#[test]
fn reap_trash_resumes() {
    let tmp = tempfile::tempdir().unwrap();
    let trash = tmp.path().join(".trash");
    fs::create_dir_all(trash.join("a-1/deep")).unwrap();
    fs::write(trash.join("a-1/deep/f"), "x").unwrap();
    fs::create_dir_all(trash.join("b-2")).unwrap();
    assert!(reap_trash(&trash).is_empty());
    assert_eq!(fs::read_dir(&trash).unwrap().count(), 0);
}

#[test]
fn setup_after_create_end_to_end() {
    let (tmp, repo) = fixture();
    fs::create_dir_all(repo.join(".vibeke")).unwrap();
    fs::write(
        repo.join(".vibeke/setup.sh"),
        "echo \"p=$PORT\" > setup.out\n",
    )
    .unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-q", "-m", "setup"]);
    let co = create_worktree(
        &req(&repo, "e2e"),
        &cfg(WorktreeRoot::Dir(tmp.path().join("wts"))),
    )
    .unwrap();
    let pl = PortLeases::new(tmp.path().join("state"), pool());
    let lease = pl.lease(&LeaseRequest::new("k1", "s")).unwrap();
    let mut o = setup_opts(&co.path, &tmp.path().join("setup.log"), Some(lease.clone()));
    o.script = DEFAULT_SETUP_SCRIPT.into();
    let out = run_setup(&o, &CancelToken::new()).unwrap();
    assert_eq!(out.status, SetupStatus::Succeeded);
    assert_eq!(
        fs::read_to_string(co.path.join("setup.out"))
            .unwrap()
            .trim(),
        format!("p={}", lease.start)
    );
}
