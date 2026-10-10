//! Cloud sandboxes (spec 17 §5, §6.3) against the `fake` provider: provider listing, sign-in
//! with a pasted token, the `needs_auth` error every client turns into the sign-in prompt, and
//! the box API without a credential. The credential lives in a file keychain
//! (`[security] keychain = "file:…"`), never the user's.

use serde_json::{Value, json};
use std::process::Command;

struct Host {
    dir: tempfile::TempDir,
}

impl Host {
    fn new() -> Host {
        let dir = tempfile::Builder::new()
            .prefix("vkcloud")
            .tempdir_in("/tmp")
            .unwrap();
        std::fs::create_dir_all(dir.path().join("fake")).unwrap();
        let kc = dir.path().join("keychain.json");
        std::fs::write(
            dir.path().join("config.toml"),
            format!(
                "[security]\nkeychain = \"file:{}\"\n\n[cloud]\ndefault_provider = \"fake\"\n",
                kc.display()
            ),
        )
        .unwrap();
        Host { dir }
    }

    fn cmd(&self, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_vibeke"));
        let d = self.dir.path();
        c.env("VIBEKE_RUNTIME_DIR", d.join("run"))
            .env("VIBEKE_STATE_DIR", d.join("state"))
            .env("VIBEKE_CONFIG", d.join("config.toml"))
            .env("VIBEKE_GATEWAY_DIR", d.join("gateway"))
            .env("VIBEKE_CLOUD_FAKE_DIR", d.join("fake"));
        for k in [
            "VIBEKE",
            "VIBEKE_SOCKET",
            "VIBEKE_SESSION",
            "VIBEKE_PANE_TOKEN",
            "SPRITES_TOKEN",
            "E2B_API_KEY",
        ] {
            c.env_remove(k);
        }
        c.arg("--json").args(args);
        c
    }

    fn api(&self, method: &str, p: Value) -> Result<Value, Value> {
        let out = self
            .cmd(&["api", "call", method, &p.to_string()])
            .output()
            .unwrap();
        if out.status.success() {
            Ok(serde_json::from_slice(&out.stdout).unwrap_or(Value::Null))
        } else {
            Err(serde_json::from_slice(&out.stderr).unwrap_or_else(
                |_| json!({"raw": String::from_utf8_lossy(&out.stderr).to_string()}),
            ))
        }
    }

    fn fake(&self, providers: &Value) -> Value {
        providers["providers"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["id"] == "fake")
            .cloned()
            .unwrap_or_else(|| panic!("fake provider not listed: {providers}"))
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.cmd(&["server", "stop", "--kill-panes"]).output();
    }
}

fn kind(e: &Value) -> &str {
    e.pointer("/error/kind")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn reason(e: &Value) -> &str {
    e.pointer("/error/details/reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

fn assert_needs_auth(e: &Value, what: &str) {
    assert_eq!(kind(e), "permission_denied", "{what}: {e}");
    assert_eq!(reason(e), "needs_auth", "{what}: {e}");
    assert_eq!(
        e.pointer("/error/details/provider").and_then(Value::as_str),
        Some("fake"),
        "{what}: {e}"
    );
    assert!(
        e.pointer("/error/details/methods")
            .is_some_and(Value::is_array),
        "{what}: {e}"
    );
}

#[test]
fn providers_sign_in_and_needs_auth() {
    let h = Host::new();

    // The fake provider is listed (tests only) and nobody is signed in yet.
    let ps = h.api("cloud.providers", json!({})).unwrap();
    let fake = h.fake(&ps);
    assert_eq!(fake["default"], true, "{fake}");
    assert_eq!(fake["auth"]["state"], "missing", "{fake}");
    assert!(fake["methods"].is_array(), "{fake}");
    for p in ps["providers"].as_array().unwrap() {
        assert!(p["caps"].is_object() && p["label"].is_string(), "{p}");
    }

    // Without a credential every provider-backed method asks for a sign-in.
    let b = "fake/vk-00000000-0000000000";
    for (m, p) in [
        ("cloud.box.list", json!({"provider": "fake"})),
        ("cloud.box.suspend", json!({"box": b})),
        ("cloud.box.resume", json!({"box": b})),
        ("cloud.box.checkpoint", json!({"box": b, "note": "x"})),
        ("cloud.box.destroy", json!({"box": b})),
        ("cloud.box.adopt", json!({"box": b})),
        ("cloud.box.forget", json!({"box": b})),
        ("cloud.prune", json!({"provider": "fake", "dry_run": true})),
    ] {
        let e = h.api(m, p).unwrap_err();
        assert_needs_auth(&e, m);
    }

    // A rejected token is needs_auth too, and nothing is stored.
    let e = h
        .api(
            "cloud.auth.set",
            json!({"provider": "fake", "token": "wrong-token"}),
        )
        .unwrap_err();
    assert_needs_auth(&e, "cloud.auth.set (bad token)");
    // The token never comes back in an error.
    assert!(!e.to_string().contains("wrong-token"), "{e}");
    let fake = h.fake(&h.api("cloud.providers", json!({})).unwrap());
    assert_eq!(fake["auth"]["state"], "missing", "{fake}");

    // Unknown providers and import sources are invalid params.
    let e = h
        .api("cloud.auth.set", json!({"provider": "nope", "token": "x"}))
        .unwrap_err();
    assert_eq!(kind(&e), "invalid_params", "{e}");
    let e = h
        .api(
            "cloud.auth.import",
            json!({"provider": "fake", "source": "no-such-cli"}),
        )
        .unwrap_err();
    assert_eq!(kind(&e), "invalid_params", "{e}");

    // The good token signs in; it is stored in the configured keychain, not echoed.
    let r = h
        .api(
            "cloud.auth.set",
            json!({"provider": "fake", "token": "fake-token"}),
        )
        .unwrap();
    assert_eq!(r["provider"], "fake", "{r}");
    assert!(r["account"].is_string(), "{r}");
    assert!(!r.to_string().contains("fake-token"), "{r}");
    let kc = std::fs::read_to_string(h.dir.path().join("keychain.json")).unwrap();
    assert!(kc.contains("vibeke/cloud/fake"), "{kc}");
    let fake = h.fake(&h.api("cloud.providers", json!({"verify": true})).unwrap());
    assert_eq!(fake["auth"]["state"], "ok", "{fake}");
    assert_eq!(fake["auth"]["source"], "keychain", "{fake}");

    // Signed in, with no boxes yet.
    let l = h
        .api(
            "cloud.box.list",
            json!({"provider": "fake", "refresh": true}),
        )
        .unwrap();
    assert_eq!(l["boxes"], json!([]), "{l}");
    assert_eq!(l["errors"], json!([]), "{l}");
    let l = h.api("cloud.box.list", json!({})).unwrap();
    assert_eq!(l["boxes"], json!([]), "{l}");
    // Forgetting a box that was never recorded is not_found.
    let e = h.api("cloud.box.forget", json!({"box": b})).unwrap_err();
    assert_eq!(kind(&e), "not_found", "{e}");
    // A dry-run prune with nothing to clean.
    let pr = h
        .api("cloud.prune", json!({"provider": "fake", "dry_run": true}))
        .unwrap();
    assert_eq!(pr["candidates"], json!([]), "{pr}");

    // Signing out removes the stored credential.
    let c = h
        .api("cloud.auth.clear", json!({"provider": "fake"}))
        .unwrap();
    assert_eq!(c["cleared"], true, "{c}");
    assert_eq!(c["boxes_running"], 0, "{c}");
    let fake = h.fake(&h.api("cloud.providers", json!({})).unwrap());
    assert_eq!(fake["auth"]["state"], "missing", "{fake}");
    let e = h
        .api("cloud.box.list", json!({"provider": "fake"}))
        .unwrap_err();
    assert_needs_auth(&e, "cloud.box.list after sign-out");
}

#[test]
fn cloud_methods_are_listed_with_their_mutating_flags() {
    let h = Host::new();
    let v = h.api("api.methods", json!({})).unwrap();
    let ms = v["methods"].as_array().unwrap();
    let flag = |name: &str| {
        ms.iter()
            .find(|m| m["name"] == name)
            .map(|m| m["mutating"] == true)
            .unwrap_or_else(|| panic!("{name} not listed"))
    };
    for m in ["cloud.providers", "cloud.box.list"] {
        assert!(!flag(m), "{m}");
    }
    for m in [
        "cloud.auth.set",
        "cloud.auth.import",
        "cloud.auth.clear",
        "cloud.box.suspend",
        "cloud.box.resume",
        "cloud.box.checkpoint",
        "cloud.box.destroy",
        "cloud.box.adopt",
        "cloud.box.forget",
        "cloud.prune",
    ] {
        assert!(flag(m), "{m}");
    }
}
