//! `share.create` for watch and review shares (spec 20 §2.2, §3.5): parameter checks, the
//! GitHub paths that fail closed, and the links and invitations it makes.

use super::*;
use crate::state::{Config, Limit, StateDir};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use vk_account::fake::FakeServer;

/// A server answering `preview.get` (`pv1`/`v4`) and `pane.get`.
fn fake_server(path: std::path::PathBuf) {
    let listener = tokio::net::UnixListener::bind(&path).unwrap();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                let (r, mut w) = stream.into_split();
                let mut lines = BufReader::new(r).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let req: Value = serde_json::from_str(&line).unwrap();
                    let p = req["params"].clone();
                    let reply = match req["method"].as_str().unwrap() {
                        "client.hello" => json!({"result": {"capabilities": ["*"]}}),
                        "preview.get" => match p["preview"].as_str().unwrap_or("") {
                            "pv1" | "v4" => json!({"result": {"preview": {
                                "id": "pv1", "handle": "v4", "pane": "p1", "label": "web",
                                "url": "http://localhost:5173/", "status": "up", "port": 5173, "path": "/"}}}),
                            _ => json!({"error": {"code": -32004, "message": "no such preview",
                                                  "data": {"kind": "not_found"}}}),
                        },
                        "pane.get" => {
                            json!({"result": {"pane": {"id": p["pane"], "workspace": "w1"}}})
                        }
                        _ => json!({"result": {}}),
                    };
                    let mut out = reply;
                    out["jsonrpc"] = json!("2.0");
                    out["id"] = req["id"].clone();
                    if w.write_all(format!("{out}\n").as_bytes()).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
}

fn owner() -> Device {
    serde_json::from_value(
        json!({"id": "o1", "name": "Owner", "public": "k", "scope": "full",
                                  "paired_at": 0}),
    )
    .unwrap()
}

fn gateway(t: &tempfile::TempDir, account_url: &str) -> Arc<Gateway> {
    let sock = t.path().join("s.sock");
    fake_server(sock.clone());
    let st = StateDir::open(t.path().join("gw")).unwrap();
    st.save_config(&Config {
        relay: Some("ws://relay.test".into()),
        app_url: Some("https://app.test".into()),
        account_url: Some(account_url.into()),
        host_name: Some("devbox".into()),
        ..Config::default()
    })
    .unwrap();
    Gateway::new(st, crate::server::Server::new(sock)).unwrap()
}

/// Sign the gateway in to `server` with access token `a1`.
fn sign_in(gw: &Gateway, server: &str) {
    let origin = vk_account::Client::new(server)
        .unwrap()
        .server()
        .to_string();
    crate::account::store(&gw.state.dir)
        .save(&vk_account::Credential {
            server: origin,
            login: "owner".into(),
            refresh_token: "r1".into(),
            access_token: Some("a1".into()),
            access_exp: Some(now_s() + 3600),
            saved_at: now_s(),
        })
        .unwrap();
}

async fn create(gw: &Arc<Gateway>, p: Value) -> ApiResult {
    share_create_as(gw, &owner(), &p).await
}

fn kind(r: ApiResult) -> String {
    r.err().map(|e| e.kind).unwrap_or_else(|| "ok".into())
}

#[tokio::test(flavor = "multi_thread")]
async fn parameters_are_checked_before_anything_is_written() {
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, &fake.url);
    for p in [
        json!({"kind": "nope", "pane": "p1"}),
        json!({"kind": "review", "preview": "pv1"}),
        json!({"kind": "share", "pane": "p1", "access": "anyone"}),
        json!({"kind": "handoff", "access": "github", "github": ["alice"]}),
        json!({"kind": "share", "pane": "p1", "access": "link", "uses": 5}),
        json!({"kind": "share", "pane": "p1", "access": "github", "github": ["alice"], "uses": 0}),
        json!({"kind": "share", "pane": "p1", "access": "github", "github": ["alice"], "uses": 101}),
        json!({"kind": "share", "pane": "p1", "notes": "held"}),
        json!({"kind": "share", "pane": "p1", "preview": "pv1"}),
        json!({"kind": "review", "access": "link", "preview": "pv1", "notes": "later"}),
        json!({"kind": "review", "access": "link", "preview": "pv1", "pane": "p1"}),
        json!({"kind": "review", "access": "link"}),
        json!({"kind": "share"}),
    ] {
        assert_eq!(kind(create(&gw, p.clone()).await), "invalid_params", "{p}");
    }
    assert_eq!(
        kind(
            create(
                &gw,
                json!({"kind": "review", "access": "link", "preview": "gone"})
            )
            .await
        ),
        "not_found"
    );
    assert!(gw.state.pairings().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn github_shares_fail_closed() {
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, &fake.url);
    let base = json!({"kind": "share", "pane": "p1", "access": "github"});
    let with = |g: Value| {
        let mut p = base.clone();
        p["github"] = g;
        p
    };
    // An empty list, `*` mixed with names, not a list.
    for p in [
        with(json!([])),
        with(json!(["*", "alice"])),
        with(json!("alice")),
        with(json!([1])),
    ] {
        assert_eq!(kind(create(&gw, p.clone()).await), "invalid_params", "{p}");
    }
    // Not signed in (with a list, or with the account's list from the account page).
    assert_eq!(
        kind(create(&gw, with(json!(["alice"]))).await),
        "account_required"
    );
    assert_eq!(kind(create(&gw, base.clone()).await), "account_required");
    assert_eq!(
        kind(create(&gw, with(json!(["*"]))).await),
        "account_required"
    );
    sign_in(&gw, &fake.url);
    fake.with(|s| s.access = Some("a1".into()));
    // No list given and nobody on the account's list: refused, with where to add people.
    let e = create(&gw, base.clone()).await.unwrap_err();
    assert_eq!(e.kind, "invalid_params");
    assert!(e.message.contains("/account"), "{}", e.message);
    // An unknown login, and one that can't be a login.
    let e = create(&gw, with(json!(["alice", "ghost"])))
        .await
        .unwrap_err();
    assert_eq!(e.kind, "unknown_user");
    assert_eq!(e.details["login"], "ghost");
    assert_eq!(
        kind(create(&gw, with(json!(["../x"]))).await),
        "unknown_user"
    );
    // The account server refuses the token (and the refresh): sign in again.
    fake.with(|s| s.access = Some("other".into()));
    assert_eq!(
        kind(create(&gw, with(json!(["alice"]))).await),
        "account_required"
    );
    assert!(
        gw.state.pairings().is_empty(),
        "nothing written on a refusal"
    );

    // An unreachable account server.
    let t2 = tempfile::tempdir().unwrap();
    let down = "http://127.0.0.1:9";
    let gw2 = gateway(&t2, down);
    sign_in(&gw2, down);
    assert_eq!(
        kind(create(&gw2, with(json!(["alice"]))).await),
        "unavailable"
    );
    assert!(gw2.state.pairings().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn github_shares_store_ids_and_stay_open_until_they_expire() {
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, &fake.url);
    sign_in(&gw, &fake.url);
    fake.with(|s| s.access = Some("a1".into()));
    let r = create(
        &gw,
        json!({"kind": "share", "workspace": "w1", "access": "github",
               "github": ["Alice", "octocat", "alice"], "ttl_s": 7200}),
    )
    .await
    .unwrap();
    assert_eq!(r["access"], "github");
    assert_eq!(r["uses"], GITHUB_USES);
    assert_eq!(
        r["github"],
        json!([{"id": 2, "login": "alice"}, {"id": 583231, "login": "octocat"}])
    );
    let link = vk_e2e::PairingLink::parse(r["link"].as_str().unwrap()).unwrap();
    assert!(r["link"].as_str().unwrap().contains("/#/pair?d="));
    let info = link.share_info().unwrap();
    assert!(info.github());
    assert_eq!(info.uses, Some(GITHUB_USES));
    let p = gw
        .state
        .pairing(r["pid"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(
        p.exp.abs_diff(r["expires_at"].as_u64().unwrap()) <= 1,
        "open as long as the share"
    );
    assert_eq!(p.uses, GITHUB_USES);
    let sh = p.share.unwrap();
    assert_eq!(
        sh.github.iter().map(|g| g.id).collect::<Vec<_>>(),
        [2, 583231]
    );
    assert!(!sh.github_any);

    let r = create(
        &gw,
        json!({"kind": "share", "pane": "p1", "access": "github", "github": ["*"], "uses": 3}),
    )
    .await
    .unwrap();
    assert_eq!(r["github"], "*");
    assert_eq!(r["uses"], 3);
    let p = gw
        .state
        .pairing(r["pid"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(p.share.unwrap().github_any);

    // Without `access`, a watch share is today's single-use link.
    let r = create(&gw, json!({"kind": "share", "pane": "p1"}))
        .await
        .unwrap();
    assert_eq!(
        (r["access"].as_str(), r["uses"].as_u64()),
        (Some("link"), Some(1))
    );
    assert!(r.get("github").is_none());
    let p = gw
        .state
        .pairing(r["pid"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert!(p.exp <= now_s() + 15 * 60);
}

#[tokio::test(flavor = "multi_thread")]
async fn review_shares_open_one_preview() {
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, &fake.url);
    let r = create(
        &gw,
        json!({"kind": "review", "access": "link", "preview": "v4", "label": "Settings"}),
    )
    .await
    .unwrap();
    let url = r["link"].as_str().unwrap();
    assert!(url.starts_with("https://app.test/#/review?d="), "{url}");
    assert_eq!(
        r["preview"],
        json!({"id": "pv1", "handle": "v4", "label": "web", "url": "http://localhost:5173/"})
    );
    assert_eq!(r["notes"], "held", "a link review holds its notes");
    assert_eq!(r["uses"], 1);
    let sid = r["sid"].as_str().unwrap();
    assert_eq!(sid.len(), 16);
    assert!(
        sid.bytes()
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    );
    assert_ne!(sid, new_sid());
    let info = vk_e2e::PairingLink::parse(url)
        .unwrap()
        .share_info()
        .unwrap();
    assert_eq!(info.kind, "review");
    assert_eq!(info.preview.unwrap().id, "pv1");
    assert_eq!(info.sid.as_deref(), Some(sid));
    let p = gw
        .state
        .pairing(r["pid"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(p.scope, Scope::View);
    let sh = p.share.unwrap();
    assert_eq!(
        sh.limit,
        Some(Limit {
            workspace: None,
            pane: None,
            preview: Some("pv1".into())
        })
    );
    let rv = sh.review.unwrap();
    assert_eq!(
        (rv.pane.as_deref(), rv.workspace.as_deref()),
        (Some("p1"), Some("w1"))
    );
    // GitHub reviews deliver notes directly by default.
    sign_in(&gw, &fake.url);
    fake.with(|s| s.access = Some("a1".into()));
    let r = create(
        &gw,
        json!({"kind": "review", "access": "github", "github": ["alice"], "preview": "pv1"}),
    )
    .await
    .unwrap();
    assert_eq!(
        (r["notes"].as_str(), r["uses"].as_u64()),
        (Some("direct"), Some(20))
    );
    let p = gw
        .state
        .pairing(r["pid"].as_str().unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(
        p.share.unwrap().review.unwrap().owner.as_deref(),
        Some("owner")
    );
}

#[test]
fn held_notes_are_released_by_the_owner_only() {
    for m in ["browser.annotations_release", "browser.annotations_discard"] {
        assert_eq!(required_scope(m), Some(Scope::Full), "{m}");
        assert!(is_mutating(m), "{m}");
        assert!(host_wide(m), "{m}");
        assert!(kind_allows("device", m), "{m}");
        assert!(!kind_allows("share", m), "{m}");
        assert!(!kind_allows("peer", m), "{m}");
    }
}

/// Without `github`, a GitHub share takes the people on the account's sharing list (the
/// account page), as ids.
#[tokio::test(flavor = "multi_thread")]
async fn github_shares_default_to_the_account_list() {
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, &fake.url);
    sign_in(&gw, &fake.url);
    fake.with(|s| {
        s.access = Some("a1".into());
        s.sharing = vec![(2, "alice".into()), (583231, "octocat".into())];
    });
    let r = create(
        &gw,
        json!({"kind": "share", "pane": "p1", "access": "github"}),
    )
    .await
    .unwrap();
    assert_eq!(r["access"], "github");
    assert_eq!(
        r["github"],
        json!([{"id": 2, "login": "alice"}, {"id": 583231, "login": "octocat"}])
    );
}

const ACME: &str = "0b5c3f7e-1d2a-4c8b-9e6f-3a1b2c3d4e5f";
const BETA: &str = "11111111-2222-4333-8444-555555555555";

fn team(id: &str, name: &str) -> vk_account::Team {
    vk_account::Team {
        id: id.into(),
        name: name.into(),
        role: "member".into(),
        members: 2,
    }
}

/// Team names match without case, ids exactly (in any case); a team the account is not in is
/// `unknown_team`, a name two teams share is ambiguous, and each team is named once.
#[test]
fn teams_by_name_or_id() {
    let mine = [
        team(ACME, "Acme"),
        team(BETA, "Beta"),
        team("22222222-2222-4333-8444-555555555555", "beta"),
    ];
    let names = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let got = pick_teams(&names(&["ACME", ACME, &BETA.to_uppercase()]), &mine).unwrap();
    assert_eq!(
        got.iter()
            .map(|t| (t.id.as_str(), t.name.as_str()))
            .collect::<Vec<_>>(),
        [(ACME, "Acme"), (BETA, "Beta")]
    );
    let e = pick_teams(&names(&["Gamma"]), &mine).unwrap_err();
    assert_eq!(
        (e.kind.as_str(), &e.details),
        ("unknown_team", &json!({"team": "Gamma"}))
    );
    // An id of a team this account is not in.
    let e = pick_teams(&names(&["33333333-2222-4333-8444-555555555555"]), &mine).unwrap_err();
    assert_eq!(e.kind, "unknown_team");
    assert_eq!(
        pick_teams(&names(&["BETA"]), &mine).unwrap_err().kind,
        "invalid_params"
    );
    for p in [
        json!({"teams": []}),
        json!({"teams": "Acme"}),
        json!({"teams": [""]}),
        json!({"teams": [1]}),
    ] {
        assert_eq!(teams_param(&p).unwrap_err().kind, "invalid_params", "{p}");
    }
    assert_eq!(teams_param(&json!({})).unwrap(), None);
    assert_eq!(teams_param(&json!({"teams": null})).unwrap(), None);
    let many: Vec<String> = (0..=MAX_TEAMS).map(|i| format!("t{i}")).collect();
    assert!(teams_param(&json!({"teams": many})).is_err());
}

/// `share.create` with `teams` (spec 20 §2.5): stored by id with the name for display, in the
/// result, the link and `share.list`; refused for link shares, with `*`, and for unknown teams.
#[tokio::test(flavor = "multi_thread")]
async fn github_shares_for_teams() {
    use vk_account::fake::FakeTeam;
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, &fake.url);
    sign_in(&gw, &fake.url);
    fake.with(|s| {
        s.access = Some("a1".into());
        s.teams = vec![
            FakeTeam::new(ACME, "Acme", &[4242]),
            FakeTeam::new(BETA, "Beta", &[4242]),
        ];
    });
    let r = create(
        &gw,
        json!({"kind": "share", "pane": "p1", "access": "github", "teams": ["acme"]}),
    )
    .await
    .unwrap();
    assert_eq!(r["teams"], json!([{"id": ACME, "name": "Acme"}]));
    assert_eq!(r["github"], json!([]));
    let pid = r["pid"].as_str().unwrap();
    let sh = gw.state.pairing(pid).unwrap().unwrap().share.unwrap();
    assert!(sh.github.is_empty());
    assert_eq!(sh.teams.len(), 1);
    assert_eq!(sh.teams[0].id, ACME);
    let info = vk_e2e::PairingLink::parse(r["link"].as_str().unwrap())
        .unwrap()
        .share_info()
        .unwrap();
    assert_eq!(info.teams[0].name, "Acme");
    let list = crate::peers::share_list(&gw.state, &gw.devices());
    let inv = list["invitations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == pid)
        .unwrap();
    assert_eq!(inv["teams"], json!([{"id": ACME, "name": "Acme"}]));

    // People and a team together, a review by team id.
    let r = create(
        &gw,
        json!({"kind": "share", "pane": "p1", "access": "github", "github": ["alice"], "teams": [BETA]}),
    )
    .await
    .unwrap();
    assert_eq!(r["github"], json!([{"id": 2, "login": "alice"}]));
    assert_eq!(r["teams"], json!([{"id": BETA, "name": "Beta"}]));
    let r = create(
        &gw,
        json!({"kind": "review", "preview": "pv1", "access": "github", "teams": ["Beta"]}),
    )
    .await
    .unwrap();
    assert_eq!(r["teams"][0]["id"], BETA);

    // Refusals, with nothing written.
    let before = gw.state.pairings().len();
    let e = create(
        &gw,
        json!({"kind": "share", "pane": "p1", "access": "github", "teams": ["Gamma"]}),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, "unknown_team");
    assert_eq!(e.details["team"], "Gamma");
    for p in [
        json!({"kind": "share", "pane": "p1", "teams": ["Acme"]}),
        json!({"kind": "share", "pane": "p1", "access": "link", "teams": ["Acme"]}),
        json!({"kind": "share", "pane": "p1", "access": "github", "github": ["*"], "teams": ["Acme"]}),
        json!({"kind": "share", "pane": "p1", "access": "github", "teams": []}),
    ] {
        assert_eq!(kind(create(&gw, p.clone()).await), "invalid_params", "{p}");
    }
    assert_eq!(gw.state.pairings().len(), before);
}

/// Without `github` and `teams`, a GitHub share takes the account page's people and ticked
/// teams; with neither it is refused (fail closed).
#[tokio::test(flavor = "multi_thread")]
async fn github_shares_default_to_the_account_people_and_teams() {
    let fake = FakeServer::start().await;
    let t = tempfile::tempdir().unwrap();
    let gw = gateway(&t, &fake.url);
    sign_in(&gw, &fake.url);
    fake.with(|s| {
        s.access = Some("a1".into());
        s.teams = vec![vk_account::fake::FakeTeam::new(ACME, "Acme", &[4242])];
        s.sharing_teams = vec![ACME.into()];
    });
    let p = json!({"kind": "share", "pane": "p1", "access": "github"});
    // Teams only.
    let r = create(&gw, p.clone()).await.unwrap();
    assert_eq!(r["github"], json!([]));
    assert_eq!(r["teams"], json!([{"id": ACME, "name": "Acme"}]));
    // People and teams.
    fake.with(|s| s.sharing = vec![(2, "alice".into())]);
    let r = create(&gw, p.clone()).await.unwrap();
    assert_eq!(r["github"], json!([{"id": 2, "login": "alice"}]));
    assert_eq!(r["teams"][0]["id"], ACME);
    // Neither: refused, naming the account page.
    fake.with(|s| {
        s.sharing = vec![];
        s.sharing_teams = vec![];
    });
    let e = create(&gw, p).await.unwrap_err();
    assert_eq!(e.kind, "invalid_params");
    assert!(
        e.message.contains("/account") && e.message.contains("team"),
        "{}",
        e.message
    );
}
