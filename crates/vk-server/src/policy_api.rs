//! Approval policy (07 §2.9, 09 §4): the merged rule view and the rule evaluation that
//! auto-answers structured approvals.
//!
//! Rule sources, in order:
//! 1. `config` — `[[policy.rule]]` in the user's `config.toml` (read fresh on each use);
//! 2. `user` — rules added with `policy.add` (the `policy_rules` table, insertion order);
//! 3. `repo` — `[[rule]]` (or `[[policy.rule]]`) in a repository's `.vibeke/policy.toml`, used
//!    only while that repository's `.vibeke/` tree is trusted at its current digest
//!    (`policy.trust`).
//!
//! Evaluation: the first matching user rule (config, then API-added) is the user decision.
//! Repository rules can only tighten it (09 §4 rule 2): a matching repo `deny` or `ask` wins over
//! a looser user effect; a repo `allow` is ignored unless the repository was trusted with
//! `allow_policy_grants` (and then it applies only where no user rule matched — user policy
//! always wins). No match means `ask`. Policy auto-answers apply only to structured approvals,
//! never to screen-detected ones (09 §5.1 rule 6).
//!
//! Matching: `tool` (exact, or a glob with `*`), `command_regex` (needs a command),
//! `path_glob` (`*` within a path component, `**` across; an `allow` rule needs every path of
//! the action to match, a `deny`/`ask` rule any), `url_glob`, and `scope` (a path prefix the
//! action's working directory or repository must be under). A rule needs at least one
//! matcher.

use crate::Server;
use crate::api::{Ctx, R, internal, invalid, not_found, s};
use crate::core::{Tx, ulid};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use vk_proto::model::Interaction;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Rule {
    pub id: String,
    /// `config` | `user` | `repo`.
    pub source: String,
    pub repo: Option<String>,
    pub tool: Option<String>,
    pub command_regex: Option<String>,
    pub path_glob: Option<String>,
    pub url_glob: Option<String>,
    /// `allow` | `deny` | `ask`.
    pub effect: String,
    pub scope: Option<String>,
    pub note: Option<String>,
    /// Why a repo rule is not applied (an `allow` without `allow_policy_grants`).
    pub ignored: Option<String>,
    pub created_at_ms: Option<i64>,
    pub created_by: Option<String>,
}

impl Rule {
    pub fn to_json(&self) -> Value {
        let mut m = serde_json::Map::new();
        let mut put = |k: &str, v: Value| {
            if !v.is_null() {
                m.insert(k.into(), v);
            }
        };
        put("id", json!(self.id));
        put("source", json!(self.source));
        put("repo", json!(self.repo));
        let mut mm = serde_json::Map::new();
        for (k, v) in [
            ("tool", &self.tool),
            ("command_regex", &self.command_regex),
            ("path_glob", &self.path_glob),
            ("url_glob", &self.url_glob),
        ] {
            if let Some(v) = v {
                mm.insert(k.into(), json!(v));
            }
        }
        put("match", Value::Object(mm));
        put("effect", json!(self.effect));
        put("scope", json!(self.scope));
        put("note", json!(self.note));
        put("ignored", json!(self.ignored));
        put("created_at_ms", json!(self.created_at_ms));
        put("created_by", json!(self.created_by));
        Value::Object(m)
    }

    fn has_matcher(&self) -> bool {
        self.tool.is_some()
            || self.command_regex.is_some()
            || self.path_glob.is_some()
            || self.url_glob.is_some()
    }

    fn matches(&self, a: &Action, where_: Option<&Path>) -> bool {
        if !self.has_matcher() {
            return false;
        }
        if let Some(t) = &self.tool
            && !glob(t, &a.tool)
        {
            return false;
        }
        if let Some(re) = &self.command_regex {
            let Some(cmd) = &a.command else { return false };
            match regex::Regex::new(re) {
                Ok(rx) if rx.is_match(cmd) => {}
                _ => return false,
            }
        }
        if let Some(g) = &self.path_glob {
            if a.paths.is_empty() {
                return false;
            }
            let hit = |p: &String| glob(g, p);
            let ok = if self.effect == "allow" {
                a.paths.iter().all(hit)
            } else {
                a.paths.iter().any(hit)
            };
            if !ok {
                return false;
            }
        }
        if let Some(g) = &self.url_glob {
            match &a.url {
                Some(u) if glob(g, u) => {}
                _ => return false,
            }
        }
        if let Some(sc) = &self.scope {
            let Some(w) = where_ else { return false };
            if !w.starts_with(expand(sc)) {
                return false;
            }
        }
        true
    }
}

/// The action an approval asks about.
#[derive(Clone, Debug, Default)]
pub struct Action {
    pub tool: String,
    pub command: Option<String>,
    pub paths: Vec<String>,
    pub url: Option<String>,
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => crate::paths::home().join(rest),
        None if p == "~" => crate::paths::home(),
        None => PathBuf::from(p),
    }
}

/// Glob with `*` (any run of characters except `/`), `**` (anything) and `?` (one character).
/// A pattern without wildcards must match exactly.
pub fn glob(pat: &str, s: &str) -> bool {
    fn go(p: &[char], s: &[char]) -> bool {
        match p.first() {
            None => s.is_empty(),
            Some('*') if p.get(1) == Some(&'*') => {
                let rest = &p[2..];
                (0..=s.len()).any(|i| go(rest, &s[i..]))
            }
            Some('*') => {
                let rest = &p[1..];
                for i in 0..=s.len() {
                    if go(rest, &s[i..]) {
                        return true;
                    }
                    if i < s.len() && s[i] == '/' {
                        break;
                    }
                }
                false
            }
            Some('?') => !s.is_empty() && s[0] != '/' && go(&p[1..], &s[1..]),
            Some(c) => s.first() == Some(c) && go(&p[1..], &s[1..]),
        }
    }
    let p: Vec<char> = pat.chars().collect();
    let s: Vec<char> = s.chars().collect();
    go(&p, &s)
}

fn effect_str(e: &vk_config::PolicyEffect) -> String {
    format!("{e:?}").to_lowercase()
}

fn config_rules() -> Vec<Rule> {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    cfg.policy
        .rule
        .iter()
        .enumerate()
        .map(|(i, r)| Rule {
            id: format!("config:{}", i + 1),
            source: "config".into(),
            tool: r.matcher.tool.clone(),
            command_regex: r.matcher.command_regex.clone(),
            path_glob: r.matcher.path_glob.clone(),
            effect: effect_str(&r.effect),
            scope: r.scope.clone(),
            ..Rule::default()
        })
        .collect()
}

fn db_rules(server: &Server) -> Vec<Rule> {
    let rows = server.with_core(|c| c.store.policy_rules().unwrap_or_default());
    rows.into_iter()
        .filter_map(|row| {
            let mut r = rule_from_json(&row.rule).ok()?;
            r.id = row.id;
            r.source = "user".into();
            r.created_at_ms = Some(row.created_at);
            r.created_by = row.created_by;
            Some(r)
        })
        .collect()
}

/// Parse a rule from API params / stored JSON: `{match: {tool, command_regex, path_glob,
/// url_glob}, effect, scope?, note?}`, or the matcher fields at the top level.
pub fn rule_from_json(v: &Value) -> Result<Rule, String> {
    let m = v.get("match").filter(|m| m.is_object()).unwrap_or(v);
    let get = |o: &Value, k: &str| -> Result<Option<String>, String> {
        match o.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if s.is_empty() => Ok(None),
            Some(Value::String(s)) => Ok(Some(s.clone())),
            Some(_) => Err(format!("`{k}` must be a string")),
        }
    };
    let effect = get(v, "effect")?.ok_or("missing `effect` (allow, deny or ask)")?;
    if !matches!(effect.as_str(), "allow" | "deny" | "ask") {
        return Err(format!("effect `{effect}` is not allow, deny or ask"));
    }
    let r = Rule {
        tool: get(m, "tool")?,
        command_regex: get(m, "command_regex")?,
        path_glob: get(m, "path_glob")?,
        url_glob: get(m, "url_glob")?,
        effect,
        scope: get(v, "scope")?,
        note: get(v, "note")?.map(|n| n.chars().take(500).collect()),
        ..Rule::default()
    };
    if let Some(re) = &r.command_regex {
        regex::Regex::new(re).map_err(|e| format!("command_regex: {e}"))?;
    }
    if !r.has_matcher() {
        return Err("a rule needs at least one of tool, command_regex, path_glob, url_glob".into());
    }
    Ok(r)
}

fn stored_json(r: &Rule) -> Value {
    let mut v = r.to_json();
    if let Some(o) = v.as_object_mut() {
        for k in [
            "id",
            "source",
            "repo",
            "ignored",
            "created_at_ms",
            "created_by",
        ] {
            o.remove(k);
        }
    }
    v
}

/// What a repository contributes: its rules (marked `ignored` where they don't apply), and
/// whether it is trusted.
#[derive(Clone, Debug, Default)]
pub struct RepoPolicy {
    pub repo: String,
    pub file: String,
    pub exists: bool,
    pub trusted: bool,
    pub allow_policy_grants: bool,
    pub rules: Vec<Rule>,
    pub errors: Vec<String>,
}

impl RepoPolicy {
    fn to_json(&self) -> Value {
        json!({"repo": self.repo, "file": self.file, "exists": self.exists, "trusted": self.trusted,
               "allow_policy_grants": self.allow_policy_grants, "rules": self.rules.len(), "errors": self.errors})
    }
}

fn kv_map(server: &Server, scope: &str, key: &str) -> HashMap<String, String> {
    server.with_core(|c| {
        c.store
            .kv_get(scope, key)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    })
}

fn canon(p: &Path) -> String {
    p.canonicalize()
        .unwrap_or_else(|_| p.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Load and judge `<repo>/.vibeke/policy.toml`.
pub fn repo_policy(server: &Server, repo: &Path) -> RepoPolicy {
    let key = canon(repo);
    let file = repo.join(".vibeke/policy.toml");
    let mut rp = RepoPolicy {
        repo: key.clone(),
        file: file.to_string_lossy().into_owned(),
        exists: file.is_file(),
        ..RepoPolicy::default()
    };
    let digest = crate::run::vibeke_dir_digest(repo);
    rp.trusted = digest
        .as_deref()
        .is_some_and(|d| crate::run::repo_trusted(server, repo, d));
    rp.allow_policy_grants = rp.trusted
        && kv_map(server, "security", "repo_policy_grants")
            .get(&key)
            .is_some_and(|d| Some(d.as_str()) == digest.as_deref());
    if !rp.exists {
        return rp;
    }
    let text = match std::fs::read_to_string(&file) {
        Ok(t) => t,
        Err(e) => {
            rp.errors.push(format!("read: {e}"));
            return rp;
        }
    };
    let doc: toml::Value = match toml::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            rp.errors.push(format!(
                "parse: {}",
                e.message().lines().next().unwrap_or("")
            ));
            return rp;
        }
    };
    let list = doc
        .get("rule")
        .or_else(|| doc.get("policy").and_then(|p| p.get("rule")))
        .and_then(toml::Value::as_array)
        .cloned()
        .unwrap_or_default();
    for (i, t) in list.iter().enumerate() {
        let v = serde_json::to_value(t).unwrap_or(Value::Null);
        match rule_from_json(&v) {
            Ok(mut r) => {
                r.id = format!("repo:{}", i + 1);
                r.source = "repo".into();
                r.repo = Some(key.clone());
                if !rp.trusted {
                    r.ignored = Some("repository not trusted at its current digest".into());
                } else if r.effect == "allow" && !rp.allow_policy_grants {
                    r.ignored = Some(
                        "repository allow rules need `policy trust --allow-policy-grants`".into(),
                    );
                }
                rp.rules.push(r);
            }
            Err(e) => rp.errors.push(format!("rule {}: {e}", i + 1)),
        }
    }
    rp
}

/// The nearest ancestor of `dir` with a `.vibeke/` directory.
pub fn find_repo(dir: &Path) -> Option<PathBuf> {
    let mut d = Some(dir);
    while let Some(p) = d {
        if p.join(".vibeke").is_dir() {
            return Some(p.to_path_buf());
        }
        d = p.parent();
    }
    None
}

/// Result of [`evaluate`].
#[derive(Clone, Debug)]
pub struct Decision {
    pub effect: String,
    pub rule: Option<Rule>,
    pub user_rule: Option<Rule>,
    pub repo_rule: Option<Rule>,
    pub repo: Option<String>,
    pub reason: String,
}

fn strictness(e: &str) -> u8 {
    match e {
        "deny" => 2,
        "ask" => 1,
        _ => 0,
    }
}

/// Decide `action` for an approval whose working directory is `cwd` and whose repository is
/// `repo` (found from `cwd` when absent).
pub fn evaluate(server: &Server, a: &Action, cwd: Option<&Path>, repo: Option<&Path>) -> Decision {
    let repo: Option<PathBuf> = repo
        .map(Path::to_path_buf)
        .or_else(|| cwd.and_then(find_repo));
    let where_ = cwd.or(repo.as_deref());
    let user_rule = config_rules()
        .into_iter()
        .chain(db_rules(server))
        .find(|r| r.matches(a, where_));
    let rp = repo.as_deref().map(|r| repo_policy(server, r));
    let repo_rule = rp.as_ref().and_then(|rp| {
        rp.rules
            .iter()
            .filter(|r| r.ignored.is_none())
            .find(|r| r.matches(a, where_))
            .cloned()
    });
    let (effect, rule, reason) = match (&user_rule, &repo_rule) {
        (u, Some(r)) if r.effect != "allow" => {
            if u.as_ref()
                .is_some_and(|u| strictness(&u.effect) >= strictness(&r.effect))
            {
                (u.as_ref().unwrap().effect.clone(), u.clone(), "user rule")
            } else {
                (
                    r.effect.clone(),
                    Some(r.clone()),
                    "repository rule (repository policy can only tighten)",
                )
            }
        }
        (Some(u), _) => (u.effect.clone(), Some(u.clone()), "user rule"),
        (None, Some(r)) => (
            r.effect.clone(),
            Some(r.clone()),
            "repository allow rule (trusted with allow_policy_grants)",
        ),
        (None, None) => ("ask".into(), None, "no rule matched"),
    };
    Decision {
        effect,
        rule,
        user_rule,
        repo_rule,
        repo: rp.map(|r| r.repo),
        reason: reason.into(),
    }
}

/// Policy decision for a structured approval (`adapter.gate`, headless approvals): `Some((effect,
/// rule id))` for allow/deny, `None` to ask the user.
pub fn match_interaction(server: &Server, it: &Interaction) -> Option<(String, String)> {
    let a = it.action.as_ref()?;
    let action = Action {
        tool: a.tool.clone(),
        command: a.command.clone(),
        paths: a.paths.clone(),
        url: None,
    };
    let (cwd, repo) = server.with_core(|c| {
        let run = c.run(&it.run);
        let cwd = run
            .and_then(|r| r.cwd.clone())
            .or_else(|| c.pane(&it.pane).and_then(|p| p.cwd.clone()));
        let repo = run
            .and_then(|r| r.task.clone())
            .and_then(|t| c.task(&t).map(|t| t.repo_root.clone()));
        (cwd, repo)
    });
    let d = evaluate(
        server,
        &action,
        cwd.as_deref().map(Path::new),
        repo.as_deref().map(Path::new),
    );
    if d.effect == "ask" {
        return None;
    }
    Some((d.effect, d.rule.map(|r| r.id).unwrap_or_default()))
}

/// `scope` for `policy.list` / `policy.test`: a path string, or `{cwd? | repo? | pane? | run?}`.
fn scope_dirs(
    server: &Server,
    ctx: &Ctx,
    p: &Value,
) -> Result<(Option<PathBuf>, Option<PathBuf>), vk_proto::rpc::RpcError> {
    let sc = p.get("scope");
    let (mut cwd, mut repo) = (None, None);
    match sc {
        None | Some(Value::Null) => {}
        Some(Value::String(path)) => cwd = Some(PathBuf::from(path)),
        Some(o @ Value::Object(_)) => {
            if let Some(c) = s(o, "cwd") {
                cwd = Some(PathBuf::from(c));
            }
            if let Some(r) = s(o, "repo") {
                repo = Some(PathBuf::from(r));
            }
            if let Some(pane) = s(o, "pane") {
                let pane = crate::api::resolve_pane(server, ctx, Some(pane))?;
                cwd = cwd.or(pane.cwd.map(PathBuf::from));
            }
            if let Some(run) = s(o, "run") {
                let (c, t) = server.with_core(|c| {
                    let r = c.run(run);
                    (
                        r.and_then(|r| r.cwd.clone()),
                        r.and_then(|r| r.task.clone())
                            .and_then(|t| c.task(&t).map(|t| t.repo_root.clone())),
                    )
                });
                cwd = cwd.or(c.map(PathBuf::from));
                repo = repo.or(t.map(PathBuf::from));
            }
        }
        Some(_) => return Err(invalid("scope must be a path or {cwd|repo|pane|run}")),
    }
    Ok((cwd, repo))
}

fn list(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let (cwd, repo) = scope_dirs(server, ctx, p)?;
    let mut rules: Vec<Value> = config_rules().iter().map(Rule::to_json).collect();
    rules.extend(db_rules(server).iter().map(Rule::to_json));
    let repos: Vec<PathBuf> = match repo.or_else(|| cwd.as_deref().and_then(find_repo)) {
        Some(r) => vec![r],
        None if p.get("scope").is_none() => {
            // No scope: every trusted repository.
            let mut v: Vec<PathBuf> = kv_map(server, "security", "repo_trust")
                .keys()
                .filter(|k| !k.contains('#'))
                .map(PathBuf::from)
                .collect();
            v.sort();
            v
        }
        None => vec![],
    };
    let mut repo_info = vec![];
    for r in repos {
        let rp = repo_policy(server, &r);
        rules.extend(rp.rules.iter().map(Rule::to_json));
        repo_info.push(rp.to_json());
    }
    Ok(json!({"rules": rules, "repos": repo_info}))
}

fn add(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let src = p.get("rule").filter(|r| r.is_object()).unwrap_or(p);
    let mut rule = rule_from_json(src).map_err(invalid)?;
    rule.id = format!("p-{}", &ulid()[18..]).to_lowercase();
    rule.source = "user".into();
    let by = crate::audit::actor_of(ctx);
    let by_s = by["client_kind"].as_str().map(str::to_string);
    let stored = stored_json(&rule);
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.policy_rule_put(&rule.id, &stored, by_s.as_deref());
        tx.event_by(
            "policy.rule_added",
            json!({"rule": rule.id}),
            by.clone(),
            json!({"rule": rule.to_json()}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    crate::audit::record(
        server,
        "policy.rule_added",
        by,
        json!({"rule": rule.id}),
        json!({"rule": rule.to_json()}),
    );
    let rule = db_rules(server)
        .into_iter()
        .find(|r| r.id == rule.id)
        .unwrap_or(rule);
    Ok(json!({"rule": rule.to_json()}))
}

fn remove(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let id = s(p, "rule_id")
        .or_else(|| s(p, "rule"))
        .or_else(|| s(p, "id"))
        .ok_or_else(|| invalid("missing param `rule_id`"))?;
    if id.starts_with("config:") || id.starts_with("repo:") {
        return Err(invalid(format!(
            "{id} comes from a file (config.toml or a repository's .vibeke/policy.toml); edit that file to remove it"
        )));
    }
    let Some(rule) = db_rules(server).into_iter().find(|r| r.id == id) else {
        return Err(not_found("policy rule", id));
    };
    let by = crate::audit::actor_of(ctx);
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.policy_rule_delete(id);
        tx.event_by(
            "policy.rule_removed",
            json!({"rule": id}),
            by.clone(),
            json!({"rule": rule.to_json()}),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    crate::audit::record(
        server,
        "policy.rule_removed",
        by,
        json!({"rule": id}),
        json!({"rule": rule.to_json()}),
    );
    Ok(json!({"removed": true, "rule": rule.to_json()}))
}

fn test(server: &Server, ctx: &Ctx, p: &Value) -> R {
    let a = p.get("action").filter(|a| a.is_object()).unwrap_or(p);
    let tool = s(a, "tool").ok_or_else(|| invalid("missing `action.tool`"))?;
    let paths: Vec<String> = match a.get("paths") {
        Some(Value::Array(v)) => v
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => s(a, "path")
            .map(|p| vec![p.to_string()])
            .unwrap_or_default(),
    };
    let action = Action {
        tool: tool.into(),
        command: s(a, "command").map(str::to_string),
        paths,
        url: s(a, "url").map(str::to_string),
    };
    let (cwd, repo) = scope_dirs(server, ctx, p)?;
    let d = evaluate(server, &action, cwd.as_deref(), repo.as_deref());
    Ok(json!({
        "effect": d.effect,
        "rule": d.rule.as_ref().map(Rule::to_json),
        "user_rule": d.user_rule.as_ref().map(Rule::to_json),
        "repo_rule": d.repo_rule.as_ref().map(Rule::to_json),
        "repo": d.repo,
        "reason": d.reason,
    }))
}

/// `policy.trust {path, digest?, allow_policy_grants?}`: the repo trust of `run::policy_trust`,
/// plus whether the repository's `allow` rules may apply (09 §4 rule 2), audited.
fn trust(server: &Server, ctx: &Ctx, p: &Value) -> R {
    // `check: true` only reports the repo file and its trust state; it records nothing.
    if p.get("check").and_then(Value::as_bool) == Some(true) {
        return crate::repo_config::check(server, p);
    }
    let mut r = crate::run::policy_trust(server, p)?;
    let grants = p
        .get("allow_policy_grants")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let repo = r["repo"].as_str().unwrap_or_default().to_string();
    let digest = r["digest"].as_str().map(str::to_string);
    let mut map = kv_map(server, "security", "repo_policy_grants");
    match (&digest, grants) {
        (Some(d), true) => {
            map.insert(repo.clone(), d.clone());
        }
        _ => {
            map.remove(&repo);
        }
    }
    {
        let mut c = server.core.lock().unwrap();
        let mut tx = Tx::new();
        tx.m.kv(
            "security",
            "repo_policy_grants",
            Some(serde_json::to_string(&map).unwrap_or_default()),
        );
        server.commit(&mut c, tx).map_err(internal)?;
    }
    let rp = repo_policy(server, Path::new(&repo));
    r["allow_policy_grants"] = json!(grants && digest.is_some());
    r["policy_rules"] = json!(rp.rules.iter().map(Rule::to_json).collect::<Vec<_>>());
    if !rp.errors.is_empty() {
        r["policy_errors"] = json!(rp.errors);
    }
    crate::audit::record(
        server,
        "policy.repo_trusted",
        crate::audit::actor_of(ctx),
        json!({"repo": repo}),
        json!({"digest": digest, "devcontainer_digest": r["devcontainer"]["digest"], "allow_policy_grants": grants && digest.is_some(), "policy_rules": rp.rules.len()}),
    );
    Ok(r)
}

pub fn api(server: &Server, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "policy.list" => list(server, ctx, p),
        "policy.add" => add(server, ctx, p),
        "policy.remove" => remove(server, ctx, p),
        "policy.test" => test(server, ctx, p),
        "policy.trust" => trust(server, ctx, p),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob("Bash", "Bash"));
        assert!(!glob("Bash", "bash"));
        assert!(glob("mcp__*", "mcp__github__create"));
        assert!(glob("src/*.rs", "src/main.rs"));
        assert!(!glob("src/*.rs", "src/a/main.rs"));
        assert!(glob("src/**", "src/a/main.rs"));
        assert!(glob("**/.env", "/home/u/app/.env"));
        assert!(glob(
            "https://*.example.com/**",
            "https://api.example.com/v1/x"
        ));
        assert!(glob("?.txt", "a.txt"));
    }

    #[test]
    fn rule_parsing_and_matching() {
        assert!(
            rule_from_json(&json!({"effect": "allow"})).is_err(),
            "no matcher"
        );
        assert!(rule_from_json(&json!({"tool": "Bash", "effect": "maybe"})).is_err());
        assert!(rule_from_json(&json!({"command_regex": "(", "effect": "deny"})).is_err());
        let r = rule_from_json(
            &json!({"match": {"tool": "Bash", "command_regex": "^npm test"}, "effect": "allow"}),
        )
        .unwrap();
        let a = |cmd: &str| Action {
            tool: "Bash".into(),
            command: Some(cmd.into()),
            ..Action::default()
        };
        assert!(r.matches(&a("npm test --watch"), None));
        assert!(!r.matches(&a("rm -rf /"), None));
        // allow path rules need every path inside; deny rules any.
        let allow = rule_from_json(&json!({"path_glob": "src/**", "effect": "allow"})).unwrap();
        let deny = rule_from_json(&json!({"path_glob": "**/.env", "effect": "deny"})).unwrap();
        let edit = Action {
            tool: "Edit".into(),
            paths: vec!["src/a.rs".into(), "app/.env".into()],
            ..Action::default()
        };
        assert!(!allow.matches(&edit, None));
        assert!(deny.matches(&edit, None));
        // scope is a path prefix.
        let scoped =
            rule_from_json(&json!({"tool": "Bash", "effect": "allow", "scope": "/work/app"}))
                .unwrap();
        assert!(scoped.matches(&a("ls"), Some(Path::new("/work/app/sub"))));
        assert!(!scoped.matches(&a("ls"), Some(Path::new("/work/other"))));
        assert!(!scoped.matches(&a("ls"), None));
    }
}
