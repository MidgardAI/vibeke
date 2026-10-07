//! Incoming handoffs (16 §15.2): the gateway's hand-over, the automatic-import policy, accepting
//! into a clone or a fresh clone, failures that keep the bundle, decline, resume and the sweep.

use super::*;
use crate::api::dispatch;
use crate::hardening::testkit::{server, user};
use std::path::{Path, PathBuf};

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?} in {}: {}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn gateway() -> Ctx {
    Ctx {
        client_id: "c-gateway".into(),
        kind: "gateway".into(),
        pane_scope: None,
        remote: false,
    }
}

struct Fx {
    _t: tempfile::TempDir,
    root: PathBuf,
    /// The sender's checkout (bundles are packed from it).
    seed: PathBuf,
    /// The shared remote (a bare repository; its path is the manifest's origin).
    origin: PathBuf,
    /// The receiver's clone of `origin`.
    clone: PathBuf,
    head: String,
    s: Arc<Server>,
    n: std::cell::Cell<u32>,
}

fn fixture(session: &str) -> Fx {
    let t = tempfile::tempdir().unwrap();
    let root = t.path().canonicalize().unwrap();
    let seed = root.join("seed");
    std::fs::create_dir_all(seed.join("app")).unwrap();
    sh(&seed, &["init", "-q", "-b", "main"]);
    std::fs::write(seed.join("app/a.txt"), "one\n").unwrap();
    sh(&seed, &["add", "-A"]);
    sh(&seed, &["commit", "-qm", "base"]);
    let head = sh(&seed, &["rev-parse", "HEAD"]);
    // Untracked on the sender's side; travels in the bundle.
    std::fs::write(seed.join("app/new.txt"), "new\n").unwrap();
    sh(&root, &["clone", "-q", "--bare", "seed", "origin.git"]);
    sh(&root, &["clone", "-q", "origin.git", "clone"]);
    let srv = root.join("srv");
    std::fs::create_dir_all(&srv).unwrap();
    let s = server(&srv, session);
    Fx {
        root: root.clone(),
        seed,
        origin: root.join("origin.git"),
        clone: root.join("clone"),
        head,
        s,
        n: std::cell::Cell::new(0),
        _t: t,
    }
}

const PATCH: &str = "diff --git a/app/a.txt b/app/a.txt\n--- a/app/a.txt\n+++ b/app/a.txt\n@@ -1 +1,2 @@\n one\n+two\n";

impl Fx {
    /// Pack a bundle on branch `branch` with `patch` as the uncommitted changes. Returns the
    /// bundle path (in the "gateway's" directory), its manifest and sha256.
    fn bundle(&self, branch: &str, patch: &str) -> (PathBuf, Manifest, String) {
        self.bundle_of(branch, patch, None)
    }

    /// [`Fx::bundle`] of the sender's job `job` (`manifest.source_job`).
    fn bundle_of(
        &self,
        branch: &str,
        patch: &str,
        job: Option<&str>,
    ) -> (PathBuf, Manifest, String) {
        self.n.set(self.n.get() + 1);
        let n = self.n.get();
        let work = self.root.join(format!("pack-{n}"));
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("changes.patch"), patch).unwrap();
        let m = Manifest {
            v: 1,
            source_host: "marvin".into(),
            repo_name: "seed".into(),
            origin: Some(self.origin.display().to_string()),
            branch: Some(branch.into()),
            head: self.head.clone(),
            bundle: "none".into(),
            cwd_rel: "app".into(),
            source_cwd: self.seed.join("app").display().to_string(),
            source_root: self.seed.display().to_string(),
            untracked: vec!["app/new.txt".into()],
            created_at: 1,
            source_job: job.map(str::to_string),
            ..Default::default()
        };
        let gw = self.root.join("gw");
        std::fs::create_dir_all(&gw).unwrap();
        let out = gw.join(format!("{n}.in.tar.zst"));
        vk_handoff::pack(&out, &work, &self.seed, &m, false).unwrap();
        let (_, sha) = hash_file(&out).unwrap();
        (out, m, sha)
    }

    async fn add(&self, branch: &str, owner: &str) -> Result<Value, RpcError> {
        let (path, m, sha) = self.bundle(branch, PATCH);
        self.add_bundle(&path, &m, &sha, owner).await
    }

    async fn add_bundle(
        &self,
        path: &Path,
        m: &Manifest,
        sha: &str,
        owner: &str,
    ) -> Result<Value, RpcError> {
        dispatch(
            &self.s,
            &gateway(),
            "handoff.incoming.add",
            &json!({"path": path, "manifest": m, "sha256": sha,
                    "from": {"host": "marvin", "owner": owner}, "actor": "gateway:phone"}),
        )
        .await
    }

    async fn add_from(
        &self,
        path: &Path,
        m: &Manifest,
        sha: &str,
        from: Value,
    ) -> Result<Value, RpcError> {
        dispatch(
            &self.s,
            &gateway(),
            "handoff.incoming.add",
            &json!({"path": path, "manifest": m, "sha256": sha, "from": from, "actor": "gateway:peer"}),
        )
        .await
    }

    async fn call(&self, method: &str, p: Value) -> Result<Value, RpcError> {
        dispatch(&self.s, &user(), method, &p).await
    }

    fn set_pref(&self, key: &str, v: Value) {
        let mut c = self.s.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv("prefs", key, Some(v.to_string()));
        self.s.commit(&mut c, tx).unwrap();
    }

    fn remember(&self, parent: &Path) {
        let mut placement = BTreeMap::new();
        placement.insert(
            origin_key(&self.origin.display().to_string()),
            Placement {
                repo: self.clone.clone(),
                worktree_parent: parent.to_path_buf(),
            },
        );
        self.set_pref("handoff.placement", json!(placement));
    }

    fn events(&self, kind: &str) -> Vec<vk_store::Event> {
        self.s.with_core(|c| {
            c.store
                .events_after(0, 10_000, &[kind.to_string()])
                .unwrap()
        })
    }

    async fn settled(&self, id: &str) -> Incoming {
        for _ in 0..600 {
            let r = load(&self.s, id).unwrap();
            if r.state != "importing" {
                return r;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("handoff {id} still importing");
    }
}

fn id_of(v: &Value) -> String {
    v["incoming"]["id"].as_str().unwrap().to_string()
}

#[test]
fn the_auto_import_policy_matrix() {
    let repo = PathBuf::from("/r/clone");
    let other = PathBuf::from("/r/other");
    let remembered = Placement {
        repo: repo.clone(),
        worktree_parent: PathBuf::from("/wt"),
    };
    for owner in ["self", "teammate"] {
        for clones in [vec![], vec![repo.clone()]] {
            for rem in [None, Some(&remembered)] {
                for always_ask in [false, true] {
                    let got = auto_placement(owner, always_ask, &clones, rem);
                    let want =
                        owner == "self" && !clones.is_empty() && rem.is_some() && !always_ask;
                    assert_eq!(
                        got.is_some(),
                        want,
                        "{owner} clones={clones:?} remembered={} always_ask={always_ask}",
                        rem.is_some()
                    );
                    if want {
                        assert_eq!(got.unwrap(), (repo.clone(), PathBuf::from("/wt")));
                    }
                }
            }
        }
    }
    // The remembered repository wins among several clones; a vanished one gives way to another.
    let both = [other.clone(), repo.clone()];
    assert_eq!(
        auto_placement("self", false, &both, Some(&remembered))
            .unwrap()
            .0,
        repo
    );
    assert_eq!(
        auto_placement(
            "self",
            false,
            std::slice::from_ref(&other),
            Some(&remembered)
        )
        .unwrap()
        .0,
        other
    );
}

#[test]
fn origins_share_a_placement_key() {
    assert_eq!(
        origin_key("git@github.com:demo/vibeke.git"),
        origin_key("https://github.com/MidgardAI/vibeke")
    );
    assert_eq!(origin_key("/srv/repo.git"), origin_key("file:///srv/repo"));
}

#[tokio::test]
async fn only_the_gateway_hands_over_and_the_bundle_is_checked() {
    let fx = fixture("ho-add");
    let (path, m, sha) = fx.bundle("feature", PATCH);

    // Not a gateway client.
    let e = dispatch(
        &fx.s,
        &user(),
        "handoff.incoming.add",
        &json!({"path": path, "manifest": m, "sha256": sha, "from": {"host": "marvin", "owner": "self"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.data.kind, "permission_denied");

    // Wrong checksum; a manifest that is not the one inside.
    let bad = "0".repeat(64);
    let e = fx.add_bundle(&path, &m, &bad, "self").await.unwrap_err();
    assert_eq!(e.data.kind, "conflict", "{e}");
    let mut other = m.clone();
    other.branch = Some("main".into());
    let e = fx
        .add_bundle(&path, &other, &sha, "self")
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict", "{e}");
    let e = fx.add_bundle(&path, &m, &sha, "boss").await.unwrap_err();
    assert_eq!(e.data.kind, "invalid_params");
    assert!(all(&fx.s).is_empty());
    let dir = in_dir(&fx.s).unwrap();
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "refused bundles leave nothing behind"
    );

    // Accepted: pending (nothing remembered), announced, notified, stored privately.
    let r = fx.add_bundle(&path, &m, &sha, "self").await.unwrap();
    let id = id_of(&r);
    assert_eq!(r["incoming"]["state"], "pending");
    assert_eq!(r["incoming"]["from"]["owner"], "self");
    assert_eq!(r["incoming"]["manifest"]["branch"], "feature");
    assert_eq!(r["incoming"]["manifest"]["untracked"], 1);
    let stored = PathBuf::from(r["incoming"]["bundle_path"].as_str().unwrap());
    assert_eq!(stored, dir.join(format!("{id}.tar.zst")));
    assert!(stored.is_file());
    assert!(path.is_file(), "the gateway deletes its own copy");
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }
    let ev = fx.events("handoff.incoming");
    assert_eq!(ev.len(), 1);
    assert_eq!(ev[0].data["incoming"]["id"], id.as_str());
    let n = fx.call("notification.list", json!({})).await.unwrap();
    assert!(
        n["notifications"]
            .as_array()
            .unwrap()
            .iter()
            .any(|x| x["title"] == "Incoming handoff from marvin: feature"),
        "{n}"
    );
    for e in &ev {
        let v = serde_json::to_value(e).unwrap();
        let problems = crate::api_schema::validate_event(&v);
        assert!(problems.is_empty(), "{problems:?}");
    }

    // Handing the same bundle over again (a lost answer) is the same record.
    let again = fx.add_bundle(&path, &m, &sha, "self").await.unwrap();
    assert_eq!(id_of(&again), id);
    assert_eq!(all(&fx.s).len(), 1);

    let list = fx.call("handoff.incoming.list", json!({})).await.unwrap();
    assert_eq!(list["incoming"].as_array().unwrap().len(), 1);
    let problems = crate::api_schema::validate_result("handoff.incoming.list", &list);
    assert!(problems.is_empty(), "{problems:?}");

    // Pane tokens never see incoming handoffs.
    let e = dispatch(
        &fx.s,
        &crate::hardening::testkit::pane_ctx("p1"),
        "handoff.incoming.list",
        &json!({}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.data.kind, "permission_denied");
}

#[tokio::test]
async fn suggestions_come_from_open_workspaces() {
    let fx = fixture("ho-suggest");
    let r = fx.add("feature/x", "teammate").await.unwrap();
    let id = id_of(&r);
    let g = fx
        .call("handoff.incoming.get", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(g["suggested"]["repos"], json!([]));
    assert_eq!(g["suggested"]["worktree_path"], Value::Null);

    // A workspace in a worktree of the clone: the main checkout is the suggested repository.
    let linked = fx.root.join("linked");
    sh(
        &fx.clone,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "side",
            linked.to_str().unwrap(),
        ],
    );
    fx.s.with_core(|c| {
        c.model.workspaces.push(vk_proto::model::Workspace {
            id: "w1".into(),
            handle: "w1".into(),
            name: None,
            auto_name: "linked".into(),
            root_path: linked.display().to_string(),
            task: None,
            order: 1.0,
            branch: None,
        })
    });
    let g = fx
        .call("handoff.incoming.get", json!({"id": id}))
        .await
        .unwrap();
    assert_eq!(g["suggested"]["repos"], json!([fx.clone]));
    assert_eq!(g["suggested"]["repo"], json!(fx.clone));
    assert_eq!(
        g["suggested"]["worktree_path"],
        json!(fx.root.join("clone-handoff-feature-x"))
    );
    assert_eq!(g["suggested"]["branch"], "handoff/feature-x");
    let problems = crate::api_schema::validate_result("handoff.incoming.get", &g);
    assert!(problems.is_empty(), "{problems:?}");
}

#[tokio::test]
async fn own_handoffs_import_automatically_only_with_a_remembered_place() {
    let fx = fixture("ho-auto");
    let parent = fx.root.join("wts");
    std::fs::create_dir_all(&parent).unwrap();

    // Nothing remembered: pending, even from one's own host with a known clone.
    fx.set_pref("handoff.repos", json!([fx.clone]));
    let r = fx.add("one", "self").await.unwrap();
    assert_eq!(r["incoming"]["state"], "pending");

    // Remembered: imported at once, at the remembered place.
    fx.remember(&parent);
    let r = fx.add("two", "self").await.unwrap();
    assert_eq!(r["incoming"]["state"], "importing");
    let done = fx.settled(&id_of(&r)).await;
    assert_eq!(done.state, "imported", "{:?}", done.error);
    let res = done.result.clone().unwrap();
    let wt = parent.join("clone-handoff-two");
    assert_eq!(res["worktree"], json!(wt));
    assert_eq!(res["branch"], "handoff/two");
    assert_eq!(
        std::fs::read_to_string(wt.join("app/a.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("app/new.txt")).unwrap(),
        "new\n"
    );
    assert!(done.bundle_path.is_empty(), "an imported bundle is dropped");
    assert_eq!(
        std::fs::read_dir(in_dir(&fx.s).unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tar.zst"))
            .count(),
        1,
        "only the pending bundle is left"
    );

    // A teammate's handoff always waits.
    let r = fx.add("three", "teammate").await.unwrap();
    assert_eq!(r["incoming"]["state"], "pending");

    // So does one's own while the user asked to be asked.
    let p = fx
        .call("handoff.prefs", json!({"always_ask": true}))
        .await
        .unwrap();
    assert_eq!(p["always_ask"], true);
    let r = fx.add("four", "self").await.unwrap();
    assert_eq!(r["incoming"]["state"], "pending");
    let p = fx.call("handoff.prefs", json!({})).await.unwrap();
    assert_eq!(p["always_ask"], true);
    let problems = crate::api_schema::validate_result("handoff.prefs", &p);
    assert!(problems.is_empty(), "{problems:?}");
}

#[tokio::test]
async fn accept_into_a_clone_checks_its_remotes() {
    let fx = fixture("ho-path");
    let r = fx.add("feature", "teammate").await.unwrap();
    let id = id_of(&r);

    // A repository whose remotes are elsewhere.
    let other = fx.root.join("other");
    std::fs::create_dir(&other).unwrap();
    sh(&other, &["init", "-q", "-b", "main"]);
    sh(
        &other,
        &["remote", "add", "origin", "https://example.com/other.git"],
    );
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": other}, "start_agent": false}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
    assert!(e.message.starts_with("repo_mismatch"), "{e}");
    assert_eq!(e.data.details["reason"], "repo_mismatch");
    assert_eq!(
        e.data.details["remotes"],
        json!(["https://example.com/other.git"])
    );
    let rec = load(&fx.s, &id).unwrap();
    assert_eq!(rec.state, "failed");
    assert!(Path::new(&rec.bundle_path).is_file(), "kept for a retry");

    // Not a repository at all; a relative path.
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.root.join("nope")}}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "not_found");
    let e = fx
        .call("handoff.accept", json!({"id": id, "repo": {"path": "rel"}}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "invalid_params");

    // Any remote may match, not just `origin`.
    sh(&fx.clone, &["remote", "rename", "origin", "upstream"]);
    let wt = fx.root.join("mine");
    let r = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.clone}, "worktree_path": wt,
                   "branch": "me/topic", "start_agent": false, "trust": ["mise", "direnv"]}),
        )
        .await
        .unwrap();
    assert_eq!(r["incoming"]["state"], "imported");
    let res = &r["incoming"]["result"];
    assert_eq!(res["worktree"], json!(wt));
    assert_eq!(res["branch"], "me/topic");
    assert_eq!(res["repo"], json!(fx.clone));
    assert_eq!(res["cwd"], json!(wt.join("app")));
    assert_eq!(res["resumed"], false);
    assert_eq!(
        res["trust"],
        json!([{"tool": "mise", "status": "not_needed"}, {"tool": "direnv", "status": "not_needed"}])
    );
    assert_eq!(
        std::fs::read_to_string(wt.join("app/a.txt")).unwrap(),
        "one\ntwo\n"
    );
    assert_eq!(sh(&wt, &["rev-parse", "--abbrev-ref", "HEAD"]), "me/topic");
    let problems = crate::api_schema::validate_result("handoff.accept", &r);
    assert!(problems.is_empty(), "{problems:?}");

    // Idempotent: accepting again returns the same import.
    let again = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.clone}, "start_agent": false}),
        )
        .await
        .unwrap();
    assert_eq!(again["incoming"]["result"], r["incoming"]["result"]);
    assert!(!fx.root.join("clone-handoff-feature").exists());

    // The placement is remembered for the origin.
    let p = fx.call("handoff.prefs", json!({})).await.unwrap();
    let key = origin_key(&fx.origin.display().to_string());
    assert_eq!(p["placement"][&key]["repo"], json!(fx.clone));
    assert_eq!(p["placement"][&key]["worktree_parent"], json!(fx.root));

    // An imported handoff is not declined; it has no pane to resume in here.
    let e = fx
        .call("handoff.decline", json!({"id": id}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
    let e = fx
        .call("handoff.resume", json!({"id": id}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
}

#[tokio::test]
async fn accept_into_a_fresh_clone() {
    let fx = fixture("ho-clone");
    let r = fx.add("feature", "teammate").await.unwrap();
    let id = id_of(&r);

    let busy = fx.root.join("busy");
    std::fs::create_dir(&busy).unwrap();
    std::fs::write(busy.join("x"), "x").unwrap();
    for (to, kind) in [
        (busy.clone(), "conflict"),
        (fx.root.join("missing/deep"), "invalid_params"),
    ] {
        let e = fx
            .call(
                "handoff.accept",
                json!({"id": id, "repo": {"clone_to": to}, "start_agent": false}),
            )
            .await
            .unwrap_err();
        assert_eq!(e.data.kind, kind, "{}: {e}", to.display());
    }

    let fresh = fx.root.join("fresh");
    let r = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"clone_to": fresh}, "start_agent": false}),
        )
        .await
        .unwrap();
    let res = &r["incoming"]["result"];
    assert_eq!(r["incoming"]["state"], "imported", "{r}");
    assert_eq!(res["cloned"], true);
    assert_eq!(res["repo"], json!(fresh));
    assert!(fresh.join(".git").is_dir());
    let wt = fx.root.join("fresh-handoff-feature");
    assert_eq!(res["worktree"], json!(wt));
    assert_eq!(
        std::fs::read_to_string(wt.join("app/a.txt")).unwrap(),
        "one\ntwo\n"
    );
    let phases: Vec<Value> = fx
        .events("handoff.updated")
        .iter()
        .map(|e| e.data["phase"].clone())
        .collect();
    assert!(phases.contains(&json!("cloning")), "{phases:?}");
}

#[tokio::test]
async fn a_failed_import_keeps_the_bundle_and_can_be_retried() {
    let fx = fixture("ho-retry");

    // The worktree path is taken: failed, nothing created, bundle kept.
    let r = fx.add("feature", "teammate").await.unwrap();
    let id = id_of(&r);
    let taken = fx.root.join("taken");
    std::fs::create_dir(&taken).unwrap();
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.clone}, "worktree_path": taken, "start_agent": false}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
    let rec = load(&fx.s, &id).unwrap();
    assert_eq!(rec.state, "failed");
    assert_eq!(rec.error.as_ref().unwrap()["kind"], "conflict");
    assert!(Path::new(&rec.bundle_path).is_file());

    // Retried elsewhere: imported, the bundle dropped.
    let r = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.clone}, "start_agent": false}),
        )
        .await
        .unwrap();
    assert_eq!(r["incoming"]["state"], "imported");
    assert_eq!(r["incoming"]["error"], Value::Null);
    assert!(!Path::new(&rec.bundle_path).exists());
    assert!(fx.root.join("clone-handoff-feature/app/new.txt").is_file());

    // A patch that does not apply: rolled back, failed, retryable.
    let (path, m, sha) = fx.bundle("broken", "diff --git a/x b/x\ngarbage\n@@ nope\n");
    let r = fx.add_bundle(&path, &m, &sha, "teammate").await.unwrap();
    let id = id_of(&r);
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.clone}, "start_agent": false}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
    let rec = load(&fx.s, &id).unwrap();
    assert_eq!(rec.state, "failed");
    assert!(Path::new(&rec.bundle_path).is_file());
    assert!(!fx.root.join("clone-handoff-broken").exists());
    let branches = sh(&fx.clone, &["branch", "--list", "handoff/broken"]);
    assert!(branches.is_empty(), "{branches}");

    // Something being imported can't be accepted twice at once.
    update(&fx.s, &id, None, |r| {
        r.state = "importing".into();
        Ok(())
    })
    .unwrap();
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.clone}}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
    // A restart fails it, keeping the bundle.
    start(&fx.s);
    let rec = load(&fx.s, &id).unwrap();
    assert_eq!(rec.state, "failed");
    assert!(Path::new(&rec.bundle_path).is_file());
}

#[tokio::test]
async fn decline_drops_the_bundle() {
    let fx = fixture("ho-decline");
    let r = fx.add("feature", "teammate").await.unwrap();
    let id = id_of(&r);
    let bundle = PathBuf::from(r["incoming"]["bundle_path"].as_str().unwrap());
    let d = fx.call("handoff.decline", json!({"id": id})).await.unwrap();
    assert_eq!(d["incoming"]["state"], "declined");
    assert_eq!(d["incoming"]["bundle_path"], Value::Null);
    assert!(!bundle.exists());
    // Idempotent; no longer acceptable or resumable.
    fx.call("handoff.decline", json!({"id": id})).await.unwrap();
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"path": fx.clone}}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
    let e = fx
        .call("handoff.resume", json!({"id": id}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict");
    let e = fx
        .call("handoff.decline", json!({"id": "nope"}))
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "not_found");
}

#[tokio::test]
async fn the_sweep_removes_expired_records_and_stray_files() {
    let fx = fixture("ho-sweep");
    let live = fx.add("live", "teammate").await.unwrap();
    let old = fx.add("old", "teammate").await.unwrap();
    let done = fx.add("done", "teammate").await.unwrap();
    let path = |v: &Value| PathBuf::from(v["incoming"]["bundle_path"].as_str().unwrap());
    let (live_b, old_b, done_b) = (path(&live), path(&old), path(&done));
    update(&fx.s, &id_of(&old), None, |r| {
        r.expires_at_ms = now_ms() - 1;
        Ok(())
    })
    .unwrap();
    // An imported record whose bundle is still there (say, a crash right after the import).
    update(&fx.s, &id_of(&done), None, |r| {
        r.state = "imported".into();
        Ok(())
    })
    .unwrap();
    let dir = in_dir(&fx.s).unwrap();
    std::fs::write(dir.join("stray.tar.zst"), "x").unwrap();
    std::fs::create_dir(dir.join(".work-left")).unwrap();

    // Expired records leave the list right away.
    let list = fx.call("handoff.incoming.list", json!({})).await.unwrap();
    assert_eq!(list["incoming"].as_array().unwrap().len(), 2);

    sweep(&fx.s, false);
    assert!(live_b.is_file());
    assert!(!old_b.exists());
    assert!(load(&fx.s, &id_of(&old)).is_none());
    assert!(!done_b.exists());
    assert!(load(&fx.s, &id_of(&done)).unwrap().bundle_path.is_empty());
    assert!(!dir.join("stray.tar.zst").exists());
    assert!(
        dir.join(".work-left").is_dir(),
        "work directories are only swept at start"
    );
    assert_eq!(fx.events("handoff.expired").len(), 1);

    sweep(&fx.s, true);
    assert!(!dir.join(".work-left").exists());
    assert!(live_b.is_file());
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id_of(&old), "repo": {"path": fx.clone}}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "not_found");
}

#[tokio::test]
async fn waiting_handoffs_are_limited_per_sender_and_in_bytes() {
    let fx = fixture("ho-quota");
    let alpha = json!({"host": "alpha", "owner": "teammate", "device": "dev-alpha"});
    let mut ids = Vec::new();
    for n in 0..MAX_WAITING_PER_SENDER {
        let (path, m, sha) = fx.bundle(&format!("b{n}"), PATCH);
        let r = fx.add_from(&path, &m, &sha, alpha.clone()).await.unwrap();
        ids.push(id_of(&r));
    }
    // A sixth from the same sending device is refused, whatever host name it gives.
    let (path, m, sha) = fx.bundle("b-more", PATCH);
    let renamed = json!({"host": "alpha-2", "owner": "teammate", "device": "dev-alpha"});
    let e = fx
        .add_from(&path, &m, &sha, renamed.clone())
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "rate_limited", "{e}");
    assert_eq!(e.data.details["reason"], "quota");
    assert_eq!(e.data.details["limit"], "per_sender");
    assert!(path.is_file(), "the gateway's copy is left to the gateway");
    // Someone else still gets through.
    let (p2, m2, sha2) = fx.bundle("b-other", PATCH);
    fx.add_from(&p2, &m2, &sha2, json!({"host": "beta", "owner": "self"}))
        .await
        .unwrap();
    // Declining one frees a place.
    fx.call("handoff.decline", json!({"id": ids[0]}))
        .await
        .unwrap();
    fx.add_from(&path, &m, &sha, renamed).await.unwrap();

    // All waiting bundles together are bounded in bytes.
    let records = all(&fx.s);
    let waiting: u64 = records
        .iter()
        .filter(|r| !r.bundle_path.is_empty())
        .map(|r| r.size)
        .sum();
    let gamma = Sender {
        host: "gamma".into(),
        owner: "self".into(),
        user: None,
        device: None,
    };
    assert!(check_quota(&records, &gamma, 1, (5, waiting + 1)).is_ok());
    let e = check_quota(&records, &gamma, 2, (5, waiting + 1)).unwrap_err();
    assert_eq!(e.data.kind, "rate_limited");
    assert_eq!(e.data.details["limit"], "bytes");
}

#[tokio::test]
async fn the_same_sender_job_exported_again_is_one_handoff() {
    let fx = fixture("ho-job");
    let from = json!({"host": "alpha", "owner": "teammate", "device": "dev-alpha"});
    let (p1, m1, sha1) = fx.bundle_of("feature", PATCH, Some("job-1"));
    let first = fx.add_from(&p1, &m1, &sha1, from.clone()).await.unwrap();
    // Exported again after a restart: another checksum, the same job.
    let (p2, m2, sha2) = fx.bundle_of("feature", "", Some("job-1"));
    assert_ne!(sha1, sha2);
    let again = fx.add_from(&p2, &m2, &sha2, from.clone()).await.unwrap();
    assert_eq!(id_of(&again), id_of(&first));
    assert_eq!(all(&fx.s).len(), 1);
    // Another sender's job of the same name, or another job, is another handoff.
    let other = json!({"host": "beta", "owner": "teammate", "device": "dev-beta"});
    let r = fx.add_from(&p2, &m2, &sha2, other).await.unwrap();
    assert_ne!(id_of(&r), id_of(&first));
    let (p3, m3, sha3) = fx.bundle_of("feature", "", Some("job-2"));
    let r = fx.add_from(&p3, &m3, &sha3, from).await.unwrap();
    assert_ne!(id_of(&r), id_of(&first));
    assert_eq!(all(&fx.s).len(), 3);
}

#[tokio::test]
async fn a_failed_import_into_a_fresh_clone_removes_the_clone() {
    let fx = fixture("ho-clone-fail");
    let r = fx.add("feature", "teammate").await.unwrap();
    let id = id_of(&r);
    let taken = fx.root.join("taken");
    std::fs::create_dir(&taken).unwrap();
    let fresh = fx.root.join("fresh");
    let e = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"clone_to": fresh}, "worktree_path": taken, "start_agent": false}),
        )
        .await
        .unwrap_err();
    assert_eq!(e.data.kind, "conflict", "{e}");
    assert!(!fresh.exists(), "the clone made for the import is gone");
    assert!(taken.is_dir());
    // An empty directory chosen as the clone target stays, empty.
    let empty = fx.root.join("empty");
    std::fs::create_dir(&empty).unwrap();
    fx.call(
        "handoff.accept",
        json!({"id": id, "repo": {"clone_to": empty}, "worktree_path": taken, "start_agent": false}),
    )
    .await
    .unwrap_err();
    assert!(empty.is_dir());
    assert_eq!(std::fs::read_dir(&empty).unwrap().count(), 0);
    // Retrying at the same place works.
    let r = fx
        .call(
            "handoff.accept",
            json!({"id": id, "repo": {"clone_to": fresh}, "start_agent": false}),
        )
        .await
        .unwrap();
    assert_eq!(r["incoming"]["state"], "imported", "{r}");
    assert!(fresh.join(".git").is_dir());
}
