//! Adapter polish API (04 §7.7, §10, §12.3, §13): `policy.suggest`, `agent.turn_usage`,
//! `agent.limits`, `agent.drift`, `agent.manifests_check` and `agent.manifest_pin`. One hook in
//! `agents::api`; everything else lives in the modules these call into.

use super::*;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) async fn api(server: &Arc<Server>, ctx: &Ctx, method: &str, p: &Value) -> Option<R> {
    Some(match method {
        "policy.suggest" => suggest_api(server, p),
        "agent.turn_usage" => turn_usage(server, ctx, p),
        "agent.limits" => Ok(json!({"limits": usage::limits(server)})),
        "agent.drift" => Ok(json!({"versions": arbiter::snapshot()})),
        "agent.manifests_check" => manifests_check(server, p).await,
        "agent.manifest_pin" => manifest_pin(p),
        _ => return None,
    })
}

// ---- policy suggest (04 §7.7) ---------------------------------------------------------------

/// One answered approval, reduced to what fingerprints need.
#[derive(Debug, Clone)]
pub struct Sample {
    pub harness: String,
    pub tool: String,
    /// The command, or the file path for path tools.
    pub subject: String,
    pub workspace: String,
    pub allowed: bool,
    pub at_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Suggestion {
    pub fingerprint: String,
    pub harness: String,
    pub tool: String,
    /// Normalised command prefix or path glob.
    pub subject: String,
    pub workspace: String,
    pub approvals: u32,
    pub denials: u32,
    pub last_at_ms: i64,
    /// Highest risk any approved sample had.
    pub risk: String,
    /// `Some` when a rule can express the group: `(match, scope)`.
    pub rule: Option<Value>,
    /// Why no rule is offered.
    pub blocked: Option<String>,
}

const PATH_TOOLS: &[&str] = &[
    "Edit",
    "Write",
    "MultiEdit",
    "NotebookEdit",
    "apply_patch",
    "write_file",
    "edit_file",
];

fn escape_regex(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if "\\.+*?()|[]{}^$#&-~".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Fingerprints approved at least `min_approvals` times with at most `max_denials` denials, as
/// ready-to-paste rules (04 §7.7). Pure: the caller supplies the answered approvals.
pub fn suggest(samples: &[Sample], min_approvals: u32, max_denials: u32) -> Vec<Suggestion> {
    struct Group {
        s: Suggestion,
        worst: vk_agents::Risk,
        compound: bool,
    }
    let mut groups: BTreeMap<String, Group> = BTreeMap::new();
    for x in samples {
        let ws = Path::new(&x.workspace);
        let is_path = PATH_TOOLS.contains(&x.tool.as_str());
        let fp = vk_agents::fingerprint(&x.harness, &x.tool, &x.subject, ws);
        let norm = if is_path {
            vk_agents::fingerprint::path_glob(&x.subject, ws)
        } else {
            vk_agents::fingerprint::command_prefix(&x.subject)
        };
        let (risk, _) = vk_agents::assess(
            &x.tool,
            (!is_path).then_some(x.subject.as_str()),
            &if is_path {
                vec![x.subject.clone()]
            } else {
                vec![]
            },
            Some(ws),
        );
        let g = groups.entry(fp.clone()).or_insert_with(|| Group {
            s: Suggestion {
                fingerprint: fp,
                harness: x.harness.clone(),
                tool: x.tool.clone(),
                subject: norm.clone(),
                workspace: x.workspace.clone(),
                approvals: 0,
                denials: 0,
                last_at_ms: 0,
                risk: "low".into(),
                rule: None,
                blocked: None,
            },
            worst: vk_agents::Risk::Low,
            compound: norm.contains(" && ") || norm.ends_with(" >") || norm.contains(" > "),
        });
        if x.allowed {
            g.s.approvals += 1;
            let rank = |r: vk_agents::Risk| match r {
                vk_agents::Risk::Low => 0,
                vk_agents::Risk::Unknown => 1,
                vk_agents::Risk::Medium => 2,
                vk_agents::Risk::High => 3,
            };
            if rank(risk) > rank(g.worst) {
                g.worst = risk;
            }
        } else {
            g.s.denials += 1;
        }
        g.s.last_at_ms = g.s.last_at_ms.max(x.at_ms);
    }
    let mut out: Vec<Suggestion> = groups
        .into_values()
        .filter(|g| g.s.approvals >= min_approvals && g.s.denials <= max_denials)
        .map(|mut g| {
            g.s.risk = match g.worst {
                vk_agents::Risk::Low => "low",
                vk_agents::Risk::Medium => "medium",
                vk_agents::Risk::High => "high",
                vk_agents::Risk::Unknown => "unknown",
            }
            .into();
            if g.worst == vk_agents::Risk::High {
                g.s.blocked = Some("a high-risk call was among the approvals".into());
            } else if g.compound {
                g.s.blocked = Some(
                    "compound or redirecting command: a prefix rule would be wider than what was approved"
                        .into(),
                );
            } else if PATH_TOOLS.contains(&g.s.tool.as_str()) {
                g.s.rule = Some(json!({"tool": g.s.tool, "path_glob": g.s.subject}));
            } else if g.s.subject.is_empty() {
                g.s.blocked = Some("no command prefix".into());
            } else {
                g.s.rule = Some(json!({
                    "tool": g.s.tool,
                    "command_regex": format!("^{}( |$)", escape_regex(&g.s.subject)),
                }));
            }
            g.s
        })
        .collect();
    out.sort_by(|a, b| {
        b.approvals
            .cmp(&a.approvals)
            .then(b.last_at_ms.cmp(&a.last_at_ms))
            .then(a.fingerprint.cmp(&b.fingerprint))
    });
    out
}

/// The TOML to paste into `config.toml` for one suggestion.
pub fn toml_of(s: &Suggestion) -> Option<String> {
    let m = s.rule.as_ref()?.as_object()?;
    let lit = |v: &str| {
        if v.contains('\'') {
            format!("{v:?}")
        } else {
            format!("'{v}'")
        }
    };
    let mut parts = vec![];
    for k in ["tool", "command_regex", "path_glob"] {
        if let Some(v) = m.get(k).and_then(Value::as_str) {
            parts.push(format!("{k} = {}", lit(v)));
        }
    }
    Some(format!(
        "[[policy.rule]]\nmatch  = {{ {} }}\neffect = \"allow\"\nscope  = {}\n",
        parts.join(", "),
        lit(&s.workspace)
    ))
}

fn answered_samples(server: &Server, harness: Option<&str>) -> Vec<Sample> {
    let (live, closed, runs): (Vec<Interaction>, Vec<Interaction>, Vec<AgentRun>) = server
        .with_core(|c| {
            (
                c.model.interactions.clone(),
                c.store.load_closed("interaction", 5000).unwrap_or_default(),
                c.store.load_closed("run", 500).unwrap_or_default(),
            )
        });
    let mut cwd_of: HashMap<String, (String, String)> = HashMap::new();
    for r in runs {
        cwd_of.insert(
            r.id.clone(),
            (r.harness.clone(), r.cwd.clone().unwrap_or_default()),
        );
    }
    server.with_core(|c| {
        for r in &c.model.runs {
            cwd_of.insert(
                r.id.clone(),
                (r.harness.clone(), r.cwd.clone().unwrap_or_default()),
            );
        }
    });
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for it in live.into_iter().chain(closed) {
        if !seen.insert(it.id.clone()) {
            continue;
        }
        if it.kind != InteractionKind::Approval {
            continue;
        }
        // Decisions a rule already made are not evidence of what the user wants.
        if it.answered_by.as_deref() == Some("policy") {
            continue;
        }
        let Some(a) = it.action.as_ref() else {
            continue;
        };
        let allowed = match it.answer.as_ref().and_then(|x| x.decision) {
            Some(Decision::Allow | Decision::AllowAlways) => true,
            Some(Decision::Deny) => false,
            None => continue,
        };
        let Some((h, ws)) = cwd_of.get(&it.run).cloned() else {
            continue;
        };
        if harness.is_some_and(|f| f != h) {
            continue;
        }
        let subject = a
            .command
            .clone()
            .or_else(|| a.paths.first().cloned())
            .unwrap_or_default();
        if subject.is_empty() {
            continue;
        }
        out.push(Sample {
            harness: h,
            tool: a.tool.clone(),
            subject,
            workspace: ws,
            allowed,
            at_ms: it.answered_at_ms.unwrap_or(it.opened_at_ms),
        });
    }
    out
}

fn suggest_api(server: &Arc<Server>, p: &Value) -> R {
    let min = p
        .get("min_count")
        .and_then(Value::as_u64)
        .unwrap_or(3)
        .max(1) as u32;
    let max_denials = p.get("max_denials").and_then(Value::as_u64).unwrap_or(0) as u32;
    let limit = p
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(50)
        .min(500) as usize;
    let include_covered = p
        .get("include_covered")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let samples = answered_samples(server, s(p, "harness"));
    let mut out = vec![];
    for sg in suggest(&samples, min, max_denials) {
        // Already covered by a rule: nothing to add.
        let covered = sg.rule.as_ref().is_some_and(|r| {
            let action = crate::policy_api::Action {
                tool: sg.tool.clone(),
                command: r.get("command_regex").is_some().then(|| sg.subject.clone()),
                paths: r
                    .get("path_glob")
                    .is_some()
                    .then(|| {
                        let g = r["path_glob"].as_str().unwrap_or("").trim_end_matches('*');
                        format!("{}probe", if g == "./" { "" } else { g })
                    })
                    .into_iter()
                    .collect(),
                url: None,
            };
            let ws = Path::new(&sg.workspace);
            let ws = (!sg.workspace.is_empty()).then_some(ws);
            crate::policy_api::evaluate(server, &action, ws, None).effect == "allow"
        });
        if covered && !include_covered {
            continue;
        }
        out.push(json!({
            "fingerprint": sg.fingerprint, "harness": sg.harness, "tool": sg.tool,
            "subject": sg.subject, "workspace": sg.workspace,
            "approvals": sg.approvals, "denials": sg.denials, "last_at_ms": sg.last_at_ms,
            "risk": sg.risk, "rule": sg.rule, "toml": toml_of(&sg), "blocked": sg.blocked,
            "covered": covered,
        }));
        if out.len() >= limit {
            break;
        }
    }
    Ok(
        json!({"suggestions": out, "min_count": min, "max_denials": max_denials, "samples": samples.len()}),
    )
}

// ---- usage ------------------------------------------------------------------------------------

fn turn_usage(server: &Arc<Server>, ctx: &Ctx, p: &Value) -> R {
    let run = resolve_run(server, ctx, s(p, "run").or(s(p, "target")))?;
    let limit = p
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(200)
        .min(2000) as usize;
    let mut turns = tailer::records_of(server, &run.id);
    let total = turns.len();
    if turns.len() > limit {
        turns.drain(..turns.len() - limit);
    }
    let sum = |f: &dyn Fn(&tailer::TurnUsageRecord) -> u64| turns.iter().map(f).sum::<u64>();
    let cost: f64 = turns.iter().filter_map(|t| t.cost_usd).sum();
    Ok(json!({
        "run": run.id,
        "turns": turns,
        "turn_count": total,
        "totals": {
            "input": sum(&|t| t.input), "output": sum(&|t| t.output),
            "cache_read": sum(&|t| t.cache_read), "cache_write": sum(&|t| t.cache_write),
            "cost_usd": turns.iter().any(|t| t.cost_usd.is_some()).then_some(cost),
        },
        "usage": run.usage,
    }))
}

// ---- manifest channel -------------------------------------------------------------------------

async fn manifests_check(server: &Arc<Server>, p: &Value) -> R {
    let cfg = vk_config::Config::load(vk_config::config_path())
        .map(|(c, _)| c)
        .unwrap_or_default();
    let url = s(p, "url")
        .map(str::to_string)
        .unwrap_or_else(|| channel::channel_url(&cfg));
    let srv = server.clone();
    let res = tokio::task::spawn_blocking(move || {
        match channel::update(&url, &channel::root()) {
            Ok(r) => {
                let announced = channel::announce_loaded(&srv);
                let _ = manifests::reload();
                Ok(json!({"serial": r.serial, "applied": r.applied, "skipped": r.skipped, "unsigned": r.unsigned, "warnings": r.warnings, "announced": announced}))
            }
            Err(channel::ChannelError::Rollback { have, got }) => {
                Ok(json!({"serial": have, "applied": [], "unchanged": true, "index_serial": got}))
            }
            Err(e) => Err(e.to_string()),
        }
    })
    .await
    .map_err(|e| internal(e.to_string()))?;
    res.map_err(|e| err(ErrorKind::RemoteUnavailable, e))
}

fn manifest_pin(p: &Value) -> R {
    let id = crate::api::req(p, "id")?;
    let root = channel::root();
    if p.get("unpin").and_then(Value::as_bool).unwrap_or(false) {
        let had = channel::unpin(&root, id).map_err(|e| invalid(e.to_string()))?;
        return Ok(json!({"id": id, "unpinned": had}));
    }
    let pin = channel::pin(&root, id, s(p, "version")).map_err(|e| invalid(e.to_string()))?;
    Ok(json!({"id": id, "version": pin.version, "pinned_at_ms": pin.pinned_at_ms}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(tool: &str, subject: &str, allowed: bool, at: i64) -> Sample {
        Sample {
            harness: "claude".into(),
            tool: tool.into(),
            subject: subject.into(),
            workspace: "/w/proj".into(),
            allowed,
            at_ms: at,
        }
    }

    #[test]
    fn groups_by_fingerprint_and_needs_n_approvals_with_no_denials() {
        let s = [
            sample("Bash", "pnpm test", true, 1),
            sample("Bash", "pnpm test --filter web", true, 2),
            sample("Bash", "pnpm -r test", true, 3),
            sample("Bash", "pnpm install", true, 4),
            sample("Bash", "pnpm install", true, 5),
            sample("Bash", "pnpm install", true, 6),
            sample("Bash", "pnpm install", false, 7),
            sample("Bash", "ls -la", true, 8),
        ];
        let v = suggest(&s, 3, 0);
        assert_eq!(v.len(), 1, "{v:?}");
        assert_eq!(v[0].subject, "pnpm test");
        assert_eq!((v[0].approvals, v[0].denials), (3, 0));
        assert_eq!(v[0].last_at_ms, 3);
        let rule = v[0].rule.as_ref().unwrap();
        assert_eq!(rule["tool"], "Bash");
        assert_eq!(rule["command_regex"], "^pnpm test( |$)");
        // Tolerating one denial brings `pnpm install` in.
        let v = suggest(&s, 3, 1);
        assert_eq!(v.len(), 2);
        // A lower threshold brings in the single `ls`.
        assert_eq!(suggest(&s, 1, 0).len(), 2);
    }

    #[test]
    fn the_suggested_regex_matches_what_was_approved_and_nothing_wider() {
        let s = [
            sample("Bash", "cargo test -p x", true, 1),
            sample("Bash", "cargo test", true, 2),
            sample("Bash", "cargo test --all", true, 3),
        ];
        let v = suggest(&s, 3, 0);
        let re = regex::Regex::new(
            v[0].rule.as_ref().unwrap()["command_regex"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        assert!(re.is_match("cargo test -p x") && re.is_match("cargo test"));
        assert!(!re.is_match("cargo testing") && !re.is_match("cargo install x"));
        assert!(!re.is_match("echo cargo test"));
    }

    #[test]
    fn risky_and_compound_commands_never_get_a_rule() {
        let s: Vec<Sample> = (0..3)
            .flat_map(|i| {
                [
                    sample("Bash", "rm -rf build", true, i),
                    sample("Bash", "cd src && make", true, 10 + i),
                    sample("Bash", "echo hi > out.txt", true, 20 + i),
                ]
            })
            .collect();
        let v = suggest(&s, 3, 0);
        assert_eq!(v.len(), 3);
        assert!(
            v.iter().all(|x| x.rule.is_none() && x.blocked.is_some()),
            "{v:?}"
        );
        let rm = v.iter().find(|x| x.subject == "rm").unwrap();
        assert_eq!(rm.risk, "high");
    }

    #[test]
    fn path_tools_suggest_a_directory_glob() {
        let s = [
            sample("Edit", "src/a.rs", true, 1),
            sample("Edit", "/w/proj/src/b.rs", true, 2),
            sample("Edit", "src/c.rs", true, 3),
        ];
        let v = suggest(&s, 3, 0);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].rule.as_ref().unwrap()["path_glob"], "src/*");
        let toml = toml_of(&v[0]).unwrap();
        assert!(toml.contains("[[policy.rule]]") && toml.contains("path_glob = 'src/*'"));
        assert!(toml.contains("scope  = '/w/proj'"));
    }

    #[test]
    fn toml_is_paste_ready_for_command_rules() {
        let s = [
            sample("Bash", "git status", true, 1),
            sample("Bash", "git status -s", true, 2),
            sample("Bash", "git status", true, 3),
        ];
        let v = suggest(&s, 3, 0);
        let t = toml_of(&v[0]).unwrap();
        assert!(t.contains("tool = 'Bash'"));
        assert!(t.contains(r"command_regex = '^git status( |$)'"));
        assert!(t.ends_with("scope  = '/w/proj'\n"));
        // And it parses as the config the policy engine reads.
        let doc: toml::Value = toml::from_str(&t).unwrap();
        let rule = &doc["policy"]["rule"][0];
        assert_eq!(rule["effect"].as_str(), Some("allow"));
        assert!(crate::policy_api::rule_from_json(&serde_json::to_value(rule).unwrap()).is_ok());
    }

    #[test]
    fn regex_metacharacters_in_prefixes_are_escaped() {
        assert_eq!(escape_regex("a.b+c"), r"a\.b\+c");
    }
}
