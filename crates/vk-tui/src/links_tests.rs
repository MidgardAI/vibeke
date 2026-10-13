//! Link helpers: the config's GitHub list, continue links read locally (the brief, expiry,
//! refusals), how a pasted link is classified, the URL a share or review link opens, link jobs
//! (progress, finish, the incoming handoff) and the messages for the error codes spec 20 adds.

use super::*;
use crate::drafts::tests::fleet;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::json;

fn now_s() -> i64 {
    crate::time::SystemTime::now()
        .duration_since(crate::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// The `d` value of a continue link.
pub(crate) fn c_value(exp: i64) -> String {
    let v = json!({
        "v": 1, "server": "https://cloud.example", "blob": "blob1",
        "k": URL_SAFE_NO_PAD.encode([3u8; 32]), "sha256": "ab12", "size": 2048, "exp": exp,
        "brief": {"repo": "shop", "branch": "feature/cart", "harness": "claude",
                  "title": "Cart totals", "last": "Tests pass;\nnext: wire the CLI.",
                  "from": "@alice"}
    });
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(&v).unwrap())
}

fn err(kind: &str, details: Value) -> RpcErr {
    RpcErr {
        kind: kind.into(),
        message: format!("{kind} from the server"),
        details,
    }
}

#[test]
fn logins_are_cleaned_and_deduplicated_from_the_config() {
    assert_eq!(clean_login(" @Alice "), Some("Alice".into()));
    assert_eq!(clean_login("a b!"), Some("ab".into()));
    assert_eq!(clean_login("@"), None);
    let (mut app, _rxs) = fleet();
    assert!(github_default(&app).is_empty());
    app.config.sharing.github = vec!["@Alice".into(), "bob".into(), "alice".into(), " ".into()];
    let list = github_default(&app);
    assert_eq!(list, ["Alice", "bob"]);
    assert_eq!(logins_text(&list), "@Alice, @bob");
}

#[test]
fn logins_come_from_objects_or_strings() {
    assert_eq!(
        login_of(&json!({"id": 7, "login": "kim"})),
        Some("kim".into())
    );
    assert_eq!(login_of(&json!("@kim")), Some("kim".into()));
    assert_eq!(login_of(&Value::Null), None);
    assert_eq!(
        logins_of(&json!([{"id": 1, "login": "a"}, {"id": 2, "login": "b"}])),
        ["a", "b"]
    );
    assert!(logins_of(&json!("*")).is_empty());
}

#[test]
fn a_continue_link_is_read_locally_in_every_form() {
    let d = c_value(now_s() + 3600);
    for form in [
        format!("https://app.example/#/continue?d={d}"),
        d.clone(),
        format!("vibeke://continue?d={d}"),
        format!("  https://app.example/#/continue?d={d}&x=1\n"),
    ] {
        assert!(is_continue(&form), "{form}");
        let c = parse_continue(&form).unwrap_or_else(|e| panic!("{form}: {e}"));
        assert_eq!(c.repo, "shop");
        assert_eq!(c.branch, "feature/cart");
        assert_eq!(c.harness, "claude");
        assert_eq!(c.title, "Cart totals");
        assert_eq!(c.from, "@alice");
        assert_eq!(c.size, 2048);
        // Control characters of the last message are dropped.
        assert_eq!(c.last, "Tests pass;next: wire the CLI.");
    }
}

#[test]
fn the_brief_lists_what_the_link_says_and_when_it_ends() {
    let c = parse_continue(&c_value(now_s() + 2 * 3600 + 120)).unwrap();
    let lines = c.lines(now_s());
    let keys: Vec<&str> = lines.iter().map(|(k, _)| *k).collect();
    assert_eq!(
        keys,
        [
            "From",
            "Repository",
            "Branch",
            "Agent",
            "Title",
            "Last message",
            "Expires"
        ]
    );
    assert_eq!(lines[0].1, "@alice");
    assert_eq!(lines[6].1, "in 2h");
    assert!(c.refusal(now_s()).is_none());
}

#[test]
fn an_expired_or_broken_continue_link_is_refused() {
    let old = parse_continue(&c_value(now_s() - 120)).unwrap();
    assert!(old.refusal(now_s()).unwrap().contains("expired"));
    assert_eq!(
        parse_continue("nonsense").unwrap_err(),
        "not a Vibeke continue link"
    );
    let no_blob = URL_SAFE_NO_PAD.encode(br#"{"v":1,"exp":1,"k":"AAAA"}"#);
    assert!(parse_continue(&no_blob).is_err());
    let newer = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&json!({"v": 2, "blob": "b", "k": "AAAA"})).unwrap());
    assert_eq!(
        parse_continue(&newer).unwrap_err(),
        "this link needs a newer Vibeke"
    );
}

#[test]
fn a_pasted_link_is_a_continue_link_or_a_pairing_link() {
    assert!(matches!(
        classify(&format!(
            "https://app.example/#/continue?d={}",
            c_value(now_s() + 60)
        )),
        Ok(Link::Continue(_))
    ));
    assert_eq!(
        classify("  ").unwrap_err(),
        "paste an invitation link first"
    );
    assert_eq!(
        classify("nonsense").unwrap_err(),
        "not a Vibeke invitation link"
    );
    let pair = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "v": 1, "relay": "wss://relay.example", "host": "H1",
            "hk": URL_SAFE_NO_PAD.encode([1u8; 32]), "pid": "pid1",
            "psk": URL_SAFE_NO_PAD.encode([2u8; 32]), "exp": now_s() + 60, "name": "mini"
        }))
        .unwrap(),
    );
    assert!(!is_continue(&pair));
    assert!(matches!(classify(&pair), Ok(Link::Pairing(_))));
}

#[test]
fn share_and_review_links_open_at_the_app() {
    assert_eq!(
        browser_url("https://app.example/#/review?d=abc", "review"),
        "https://app.example/#/review?d=abc"
    );
    assert_eq!(
        browser_url(" abc ", "review"),
        "https://app.vibeke.dev/#/review?d=abc"
    );
    assert_eq!(
        browser_url("vibeke://pair?d=abc&x=1", "pair"),
        "https://app.vibeke.dev/#/pair?d=abc"
    );
}

#[test]
fn a_job_follows_the_status_answers_to_the_end() {
    let now = Instant::now();
    let mut j = Job::new("J1");
    assert!(j.due(now));
    j.asked = Some(now);
    assert!(!j.due(now));
    assert!(j.due(now + POLL));
    assert_eq!(j.next_at(now), now + POLL);
    assert_eq!(j.text(), "exporting the session…");
    assert_eq!(
        j.apply(&json!({"job": "J1", "state": "uploading", "progress": 0.42})),
        Finish::Running
    );
    assert_eq!(j.text(), "uploading 42%…");
    // A percentage is understood too.
    j.apply(&json!({"state": "downloading", "progress": 50}));
    assert_eq!(j.text(), "downloading 50%…");
    assert_eq!(
        j.apply(&json!({"state": "done", "incoming": "H9", "link": "L"})),
        Finish::Done
    );
    assert_eq!(j.incoming().as_deref(), Some("H9"));
    j.apply(&json!({"state": "done", "incoming": {"id": "H8"}}));
    assert_eq!(j.incoming().as_deref(), Some("H8"));
    assert_eq!(
        j.apply(&json!({"state": "failed", "error": "blob expired"})),
        Finish::Failed("blob expired".into())
    );
    assert_eq!(
        j.apply(&json!({"state": "failed"})),
        Finish::Failed("the job failed".into())
    );
}

#[test]
fn new_error_codes_read_as_sentences() {
    assert!(share_error(&err("account_required", Value::Null)).contains("vibeke login"));
    assert!(share_error(&err("unavailable", Value::Null)).contains("account server"));
    assert_eq!(
        share_error(&err("unknown_user", json!({"login": "ghost"}))),
        "@ghost is not a GitHub user: check the login"
    );
    assert_eq!(
        share_error(&err("unknown_user", Value::Null)),
        "one of the logins is not a GitHub user: check the login"
    );
    // Anything else keeps the server's words.
    assert_eq!(
        share_error(&err("permission_denied", Value::Null)),
        "permission_denied from the server"
    );
}
