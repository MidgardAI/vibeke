//! Mentions (spec 19) end to end through the real server, panes and `vibeke hook` shim. The
//! agents are fake: each pane runs a shell loop that shows a `> ` input line and reads lines,
//! so a typed envelope shows on its screen and an empty input box looks like Claude's. The test
//! reports Claude hooks for the pane with the pane's own token. Every successful call and every
//! event is checked against the API schema registry.

use crate::support::{Rpc, Session, bin};
use serde_json::{Value, json};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use vk_server::api_schema::{validate_event, validate_params, validate_result};

fn until<T>(what: &str, secs: u64, mut f: impl FnMut() -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        if let Some(v) = f() {
            return v;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// A JSON-RPC connection whose successful calls are validated against the registry.
struct Api(Rpc);

impl Api {
    fn user(s: &Session) -> Api {
        s.json(&["server", "status"]);
        Api(Rpc::connect(&s.socket()))
    }
    /// A connection with a pane's token: pane scope, like an agent in that pane.
    fn pane(s: &Session, token: &str) -> Api {
        let mut a = Api(Rpc::connect(&s.socket()));
        a.0.call(
            "client.hello",
            json!({"client": "mentions-test", "version": "0", "api": "vibeke/1", "kind": "agent", "token": token}),
        )
        .expect("hello with a pane token");
        a
    }
    fn call(&mut self, method: &str, params: Value) -> Result<Value, Value> {
        let r = self.0.call(method, params.clone());
        if let Ok(v) = &r {
            let mut probs = validate_params(method, &params);
            probs.extend(validate_result(method, v));
            assert!(
                probs.is_empty(),
                "{method}: {probs:?}\nparams {params}\nresult {v}"
            );
        }
        r
    }
    fn ok(&mut self, method: &str, params: Value) -> Value {
        match self.call(method, params.clone()) {
            Ok(v) => v,
            Err(e) => panic!("{method} {params}: {e}"),
        }
    }
    fn delivery_state(&mut self, message: &str) -> String {
        let m = self.ok("message.get", json!({"id": message}));
        m["deliveries"][0]["state"]
            .as_str()
            .unwrap_or("")
            .to_string()
    }
    /// Every event of the session matches its registered shape.
    fn check_events(&mut self) {
        let mut after = 0;
        loop {
            let v = self
                .0
                .call("events.read", json!({"after": after, "limit": 500}))
                .unwrap();
            let evs = v["events"].as_array().cloned().unwrap_or_default();
            for e in &evs {
                let probs = validate_event(e);
                assert!(probs.is_empty(), "event {e}: {probs:?}");
                after = e["seq"].as_i64().unwrap_or(after);
            }
            if evs.len() < 500 {
                return;
            }
        }
    }
}

fn kind(e: &Value) -> (String, String) {
    (
        e["data"]["kind"].as_str().unwrap_or("").to_string(),
        e["data"]["details"]["reason"]
            .as_str()
            .unwrap_or("")
            .to_string(),
    )
}

/// A pane with a `> ` input line and its token, as a fake agent pane.
struct Agent {
    pane: String,
    token: String,
}

fn agent_pane(s: &Session, tag: &str, split_of: Option<&str>) -> Agent {
    let tok = s.dir.path().join(format!("{tag}.tok"));
    let cmd = format!(
        "printf %s \"$VIBEKE_PANE_TOKEN\" > {}; printf '> '; while IFS= read -r l; do printf '> '; done",
        tok.display()
    );
    let pane = match split_of {
        Some(f) => s.split(f, &cmd),
        None => s.workspace(&cmd),
    };
    let token = until(&format!("the token of {tag}"), 20, || {
        std::fs::read_to_string(&tok).ok().filter(|t| t.len() == 64)
    });
    Agent { pane, token }
}

/// Run the real hook shim for `agent` (`vibeke hook claude <event>`); its stdout as JSON.
fn hook(s: &Session, agent: &Agent, event: &str, payload: Value) -> Value {
    let mut c = Command::new(bin());
    c.env("VIBEKE", "1")
        .env("VIBEKE_SOCKET", s.socket())
        .env("VIBEKE_PANE_TOKEN", &agent.token)
        .env_remove("VIBEKE_HEADLESS_OWNER")
        .env_remove("VIBEKE_HOOK_TEE")
        .args(["hook", "claude", event])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = c.spawn().expect("run the hook shim");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(payload.to_string().as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str(text.trim()).unwrap_or_else(|e| panic!("{e}: {text}"))
    }
}

fn session_start(s: &Session, a: &Agent, tag: &str) -> Value {
    hook(
        s,
        a,
        "SessionStart",
        json!({"session_id": format!("s-{tag}"), "source": "startup"}),
    )
}

fn prompt(s: &Session, a: &Agent, text: &str) -> Value {
    hook(
        s,
        a,
        "UserPromptSubmit",
        json!({"session_id": "s", "prompt": text}),
    )
}

fn context(out: &Value) -> String {
    out["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// Wait for the envelope of `message` on the agent's screen; its nonce.
fn envelope_nonce(user: &mut Api, a: &Agent, message: &str) -> String {
    // The line wraps in a narrow pane: match it with all whitespace removed.
    let re = regex::Regex::new(&format!(
        r"vibekemessage{}from[^\[]*\[([A-Z2-7]{{12}})\]",
        regex::escape(message)
    ))
    .unwrap();
    until(&format!("the envelope of {message}"), 20, || {
        let text: String = user.ok("pane.read", json!({"pane": a.pane, "lines": 200}))["text"]
            .as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        re.captures(&text).map(|c| c[1].to_string())
    })
}

/// Report the typed envelope through the hook, as the harness would; the hook output.
fn take_envelope(s: &Session, user: &mut Api, a: &Agent, message: &str) -> Value {
    let nonce = envelope_nonce(user, a, message);
    prompt(
        s,
        a,
        &format!("↪ vibeke message {message} from @someone · text [{nonce}]"),
    )
}

/// Two fake Claude panes in one workspace: `@coder` (a) and `@reviewer` (b), both idle.
fn pair() -> (Session, Api, Agent, Agent) {
    let s = Session::new();
    let a = agent_pane(&s, "a", None);
    let b = agent_pane(&s, "b", Some(&a.pane));
    let mut user = Api::user(&s);
    user.ok("mention.set", json!({"pane": a.pane, "name": "coder"}));
    user.ok("mention.set", json!({"pane": b.pane, "name": "reviewer"}));
    session_start(&s, &a, "a");
    let out = session_start(&s, &b, "b");
    let ctx = context(&out);
    assert!(ctx.contains("@reviewer (you)"), "{out}");
    assert!(ctx.contains("@coder · agent · claude · idle"), "{out}");
    (s, user, a, b)
}

#[test]
fn naming_rules_and_conflicts() {
    let s = Session::new();
    let a = agent_pane(&s, "a", None);
    let b = agent_pane(&s, "b", Some(&a.pane));
    let mut user = Api::user(&s);
    let m = user.ok("mention.set", json!({"pane": b.pane, "name": "reviewer"}));
    assert_eq!(m["pane"], b.pane.as_str());
    for bad in ["Reviewer", "all", "current", "-x"] {
        let e = user
            .call("mention.set", json!({"pane": a.pane, "name": bad}))
            .unwrap_err();
        assert_eq!(kind(&e).0, "invalid_params", "{bad}: {e}");
    }
    let e = user
        .call("mention.set", json!({"pane": a.pane, "name": "reviewer"}))
        .unwrap_err();
    assert_eq!(kind(&e), ("conflict".into(), "name_taken".into()), "{e}");
    // An agent names only its own pane.
    let mut pa = Api::pane(&s, &a.token);
    let e = pa
        .call("mention.set", json!({"pane": b.pane, "name": "x"}))
        .unwrap_err();
    assert_eq!(kind(&e).0, "permission_denied", "{e}");
    let mine = pa.ok("mention.set", json!({"name": "coder"}));
    assert_eq!(mine["pane"], a.pane.as_str());
    let sug = user.ok("mention.suggest", json!({"pane": b.pane}));
    assert_eq!(sug, json!({"name": "reviewer", "named": true}));
    // The roster of the workspace, and `@name` as a pane target.
    let list = pa.ok("mention.list", json!({}));
    let names: Vec<&str> = list["mentions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["coder", "reviewer"]);
    assert_eq!(list["mentions"][0]["state"], "shell");
    let read = user.ok("pane.read", json!({"pane": "@reviewer", "lines": 5}));
    assert!(read["text"].is_string());
    // Clearing, then closing the pane, remove the names.
    pa.ok("mention.clear", json!({}));
    user.ok("pane.close", json!({"pane": b.pane}));
    until("the closed pane's name to go", 15, || {
        let l = user.ok("mention.list", json!({}));
        l["mentions"].as_array().unwrap().is_empty().then_some(())
    });
    user.check_events();
}

#[test]
fn send_envelope_hook_reply_and_reply_delivery() {
    let (s, mut user, a, b) = pair();
    let mut pa = Api::pane(&s, &a.token);
    let mut pb = Api::pane(&s, &b.token);
    let sent = pa.ok(
        "message.send",
        json!({"to": ["@reviewer"], "body": "Please review the parser change.\nThe tests are in parser_tests.rs."}),
    );
    let mid = sent["message"]["id"].as_str().unwrap().to_string();
    assert_eq!(sent["deliveries"][0]["name"], "reviewer");
    // The envelope is typed into the idle recipient; its hook expands it.
    let out = take_envelope(&s, &mut user, &b, &mid);
    let ctx = context(&out);
    assert!(ctx.contains("The tests are in parser_tests.rs."), "{out}");
    assert!(ctx.contains("untrusted"), "{ctx}");
    assert!(ctx.contains("from @coder"), "{ctx}");
    assert!(ctx.contains("message_reply"), "{ctx}");
    assert!(ctx.contains("@reviewer (you)"), "{ctx}");
    assert_eq!(user.delivery_state(&mid), "delivered");
    // The inbox of the recipient; another pane's inbox is not readable.
    let inbox = pb.ok(
        "message.list",
        json!({"direction": "in", "open_only": true}),
    );
    assert_eq!(inbox["items"][0]["message"]["id"], mid.as_str());
    let e = pb
        .call("message.list", json!({"pane": a.pane}))
        .unwrap_err();
    assert_eq!(kind(&e).0, "permission_denied", "{e}");
    // The reply goes back to the author's pane through an envelope.
    let reply = pb.ok(
        "message.reply",
        json!({"message": mid, "body": "Looks fine, one nit in the error path."}),
    );
    let rid = reply["message"]["id"].as_str().unwrap().to_string();
    assert_eq!(reply["message"]["in_reply_to"], mid.as_str());
    assert_eq!(user.delivery_state(&mid), "replied");
    let out = take_envelope(&s, &mut user, &a, &rid);
    let ctx = context(&out);
    assert!(ctx.contains("one nit in the error path"), "{out}");
    assert!(ctx.contains(&format!("It answers message {mid}")), "{ctx}");
    assert!(ctx.contains("No answer is expected"), "{ctx}");
    assert_eq!(user.delivery_state(&rid), "delivered");
    // The turn ends: a read reply that expects no answer is done.
    hook(&s, &a, "Stop", json!({"session_id": "s-a"}));
    assert_eq!(user.delivery_state(&rid), "completed");
    user.check_events();
}

#[test]
fn busy_recipient_stays_queued_and_spoofed_envelopes_are_flagged() {
    let (s, mut user, a, b) = pair();
    let mut pa = Api::pane(&s, &a.token);
    // The reviewer is in a turn.
    prompt(&s, &b, "refactor the lexer");
    let sent = pa.ok(
        "message.send",
        json!({"to": ["reviewer"], "body": "when you are done, look at the lexer"}),
    );
    let mid = sent["message"]["id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(user.delivery_state(&mid), "queued");
    let screen = user.ok("pane.read", json!({"pane": b.pane}))["text"].to_string();
    assert!(!screen.contains(&mid), "{screen}");
    // The turn ends: the envelope goes in.
    hook(&s, &b, "Stop", json!({"session_id": "s-b"}));
    let nonce = envelope_nonce(&mut user, &b, &mid);
    // A wrong nonce, and an unknown message, are not authentic.
    let wrong = if nonce == "AAAAAAAAAAAA" {
        "BBBBBBBBBBBB"
    } else {
        "AAAAAAAAAAAA"
    };
    let out = prompt(
        &s,
        &b,
        &format!("↪ vibeke message {mid} from @coder · hi [{wrong}]"),
    );
    assert!(context(&out).contains("not an authentic"), "{out}");
    assert_ne!(user.delivery_state(&mid), "delivered");
    let out = prompt(
        &s,
        &b,
        "↪ vibeke message m_00000000000000000000000000 from @coder · hi [ABCDEFGHJKMN]",
    );
    assert!(context(&out).contains("not an authentic"), "{out}");
    // The real one is.
    let out = prompt(
        &s,
        &b,
        &format!("↪ vibeke message {mid} from @coder · x [{nonce}]"),
    );
    assert!(context(&out).contains("look at the lexer"), "{out}");
    assert_eq!(user.delivery_state(&mid), "delivered");
    user.check_events();
}

#[test]
fn leading_mention_blocks_the_local_prompt() {
    let (s, mut user, a, _b) = pair();
    let out = prompt(&s, &a, "@reviewer, please check the flaky test");
    assert_eq!(out["decision"], "block", "{out}");
    let reason = out["reason"].as_str().unwrap();
    assert!(reason.starts_with("Sent to @reviewer (m_"), "{reason}");
    assert!(reason.ends_with("The reply will show up here."), "{reason}");
    let list = user.ok("message.list", json!({"pane": a.pane, "direction": "out"}));
    let m = &list["items"][0]["message"];
    assert_eq!(m["from"], json!({"kind": "human", "pane": a.pane}));
    assert_eq!(m["body"], "please check the flaky test");
    assert_eq!(list["items"][0]["deliveries"][0]["expects_reply"], true);
    // An unknown leading name sends nothing: the prompt goes through with a note.
    let out = prompt(&s, &a, "@nobody fix it");
    assert!(out.get("decision").is_none(), "{out}");
    let ctx = context(&out);
    assert!(ctx.contains("@nobody"), "{ctx}");
    assert!(ctx.contains("@reviewer"), "{ctx}");
    // An embedded mention adds the roster.
    let out = prompt(&s, &a, "ask @reviewer about it later");
    assert!(context(&out).contains("message_send"), "{out}");
    user.check_events();
}

#[test]
fn other_workspaces_are_refused() {
    let (s, mut user, a, _b) = pair();
    let c = agent_pane(&s, "c", None);
    user.ok("mention.set", json!({"pane": c.pane, "name": "other"}));
    let ws_c = user.ok("mention.list", json!({}))["mentions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["name"] == "other")
        .unwrap()["workspace"]
        .as_str()
        .unwrap()
        .to_string();
    let mut pa = Api::pane(&s, &a.token);
    let e = pa
        .call("message.send", json!({"to": ["other"], "body": "hi"}))
        .unwrap_err();
    assert_eq!(kind(&e).0, "not_found", "{e}");
    assert_eq!(e["data"]["details"]["unknown"], json!(["@other"]));
    assert!(
        e["data"]["details"]["known"]
            .as_array()
            .unwrap()
            .contains(&json!("@reviewer"))
    );
    let e = pa
        .call(
            "message.send",
            json!({"to": ["other"], "body": "hi", "workspace": ws_c}),
        )
        .unwrap_err();
    assert_eq!(kind(&e).0, "permission_denied", "{e}");
    // Only people retry a held delivery.
    let sent = pa.ok("message.send", json!({"to": ["reviewer"], "body": "x"}));
    let did = sent["deliveries"][0]["id"].as_str().unwrap();
    let e = pa
        .call("message.retry", json!({"delivery": did}))
        .unwrap_err();
    assert_eq!(kind(&e).0, "permission_denied", "{e}");
    pa.ok("message.cancel", json!({"delivery": did}));
    user.check_events();
}

#[test]
fn hop_limit_ends_a_reply_chain() {
    let (s, mut user, a, b) = pair();
    let mut pa = Api::pane(&s, &a.token);
    let mut pb = Api::pane(&s, &b.token);
    let sent = pa.ok("message.send", json!({"to": ["reviewer"], "body": "ping"}));
    let mut last = sent["message"]["id"].as_str().unwrap().to_string();
    let max = 8;
    for hop in 1..=max {
        let api = if hop % 2 == 1 { &mut pb } else { &mut pa };
        let r = api.ok(
            "message.reply",
            json!({"message": last, "body": format!("pong {hop}")}),
        );
        assert_eq!(r["message"]["hop"], hop);
        last = r["message"]["id"].as_str().unwrap().to_string();
    }
    let api = if (max + 1) % 2 == 1 { &mut pb } else { &mut pa };
    let e = api
        .call(
            "message.reply",
            json!({"message": last, "body": "one too many"}),
        )
        .unwrap_err();
    assert_eq!(kind(&e), ("conflict".into(), "hop_limit".into()), "{e}");
    user.check_events();
}

#[test]
fn stop_reminds_once_then_completes() {
    let (s, mut user, a, b) = pair();
    let mut pa = Api::pane(&s, &a.token);
    let sent = pa.ok(
        "message.send",
        json!({"to": ["reviewer"], "body": "Which test covers the retry path?"}),
    );
    let mid = sent["message"]["id"].as_str().unwrap().to_string();
    take_envelope(&s, &mut user, &b, &mid);
    assert_eq!(user.delivery_state(&mid), "delivered");
    let out = hook(&s, &b, "Stop", json!({"session_id": "s-b"}));
    assert_eq!(out["decision"], "block", "{out}");
    assert!(out["reason"].as_str().unwrap().contains(&mid), "{out}");
    assert_eq!(user.delivery_state(&mid), "delivered");
    let out = hook(
        &s,
        &b,
        "Stop",
        json!({"session_id": "s-b", "stop_hook_active": true}),
    );
    assert!(out.get("decision").is_none(), "{out}");
    let m = user.ok("message.get", json!({"id": mid}));
    assert_eq!(m["deliveries"][0]["state"], "completed");
    assert_eq!(m["deliveries"][0]["reason"], "no_reply");
    // `message.complete` closes a delivery without a reply too.
    let sent = pa.ok("message.send", json!({"to": ["reviewer"], "body": "fyi"}));
    let mid2 = sent["message"]["id"].as_str().unwrap().to_string();
    let mut pb = Api::pane(&s, &b.token);
    pb.ok("message.complete", json!({"message": mid2}));
    assert_eq!(user.delivery_state(&mid2), "completed");
    user.check_events();
}

fn screen(user: &mut Api, a: &Agent) -> String {
    user.ok("pane.read", json!({"pane": a.pane, "lines": 200}))["text"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

#[test]
fn delivery_waits_for_an_empty_input_box_and_envelopes_work_once() {
    let (s, mut user, a, b) = pair();
    let mut pa = Api::pane(&s, &a.token);
    // The person has a draft in the reviewer's input box.
    let draft = "half a thought";
    user.ok(
        "pane.send_text",
        json!({"pane": b.pane, "text": draft, "paste": "raw"}),
    );
    until("the draft on the screen", 10, || {
        screen(&mut user, &b)
            .contains(&format!("> {draft}"))
            .then_some(())
    });
    let sent = pa.ok(
        "message.send",
        json!({"to": ["reviewer"], "body": "Please look at the cache keys."}),
    );
    let mid = sent["message"]["id"].as_str().unwrap().to_string();
    std::thread::sleep(Duration::from_millis(2500));
    assert_eq!(user.delivery_state(&mid), "queued");
    let text = screen(&mut user, &b);
    assert!(!text.contains(&mid), "{text}");
    // The draft is deleted: the envelope goes in on its own.
    user.ok(
        "pane.send_text",
        json!({"pane": b.pane, "text": "\u{7f}".repeat(draft.len()), "paste": "raw"}),
    );
    let nonce = envelope_nonce(&mut user, &b, &mid);
    let line = format!("↪ vibeke message {mid} from @coder · x [{nonce}]");
    let out = prompt(&s, &b, &line);
    assert!(context(&out).contains("cache keys"), "{out}");
    assert_eq!(user.delivery_state(&mid), "delivered");
    // The same line again (a replay from the screen or a transcript) carries no body.
    let out = prompt(&s, &b, &line);
    let ctx = context(&out);
    assert!(ctx.contains("already delivered"), "{out}");
    assert!(!ctx.contains("cache keys"), "{out}");
    // Events and the item stream never carry the nonce.
    let evs = user.ok("events.read", json!({"after": 0, "limit": 500}));
    assert!(!evs.to_string().contains(&nonce), "the nonce leaked into events");
    user.check_events();
}
