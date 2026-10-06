//! The combined cross-machine agent list (06 A5, `vibeke agent list --all-machines`): every
//! run across machines, sorted by attention
//! `needs_approval > needs_answer > error > done > working > idle`, with fully qualified
//! handles (`devbox/w3:p5`). Machines that cannot be reached are listed as offline; nothing
//! falls back to another machine.

use serde_json::{Value, json};

/// Attention classes in sort order.
pub const ORDER: &[&str] = &[
    "needs_approval",
    "needs_answer",
    "error",
    "done",
    "working",
    "idle",
    "exited",
    "unknown",
];

/// What one machine answered (`agent.list` and `interaction.list {status: open}` results), or
/// why it could not.
pub struct MachineAgents {
    pub machine: String,
    pub result: Result<(Value, Value), String>,
}

/// The attention class of a run (`agent.list` row) given its open interactions.
pub fn attention(run: &Value, open: &[&Value]) -> &'static str {
    let mine: Vec<&&Value> = open
        .iter()
        .filter(|i| i["run"] == run["id"] && !run["id"].is_null())
        .collect();
    if mine
        .iter()
        .any(|i| i["kind"] == "approval" || i["kind"] == "plan_review")
    {
        return "needs_approval";
    }
    if mine.iter().any(|i| i["kind"] == "question") {
        return "needs_answer";
    }
    if run["open_interactions"].as_u64().unwrap_or(0) > 0 {
        return "needs_approval";
    }
    let exec = run
        .pointer("/execution/value")
        .and_then(Value::as_str)
        .unwrap_or("Unknown")
        .to_ascii_lowercase();
    match exec.as_str() {
        "error" | "rate_limited" | "ratelimited" => "error",
        "idle" if run["done"].as_bool() == Some(true) => "done",
        "idle" => "idle",
        "working" | "starting" => "working",
        "exited" => "exited",
        _ => "unknown",
    }
}

fn rank(a: &str) -> usize {
    ORDER.iter().position(|x| *x == a).unwrap_or(ORDER.len())
}

/// Merge machines into one list: `{agents: [...], offline: [{machine, error}]}`. Rows keep the
/// machine order given (local first) within an attention class, then the start time.
pub fn combine(machines: Vec<MachineAgents>) -> Value {
    let mut rows: Vec<(usize, usize, i64, Value)> = Vec::new();
    let mut offline = Vec::new();
    for (mi, m) in machines.into_iter().enumerate() {
        let (agents, ints) = match m.result {
            Ok(x) => x,
            Err(e) => {
                offline.push(json!({"machine": m.machine, "error": e}));
                continue;
            }
        };
        let open: Vec<&Value> = ints["interactions"]
            .as_array()
            .map(|a| a.iter().collect())
            .unwrap_or_default();
        for r in agents["runs"].as_array().into_iter().flatten() {
            let att = attention(r, &open);
            let pane = r["pane_handle"].as_str().unwrap_or("");
            let mut row = r.clone();
            row["machine"] = json!(m.machine);
            row["attention"] = json!(att);
            row["qualified"] = json!(if pane.is_empty() {
                format!("{}/{}", m.machine, r["handle"].as_str().unwrap_or(""))
            } else {
                format!("{}/{pane}", m.machine)
            });
            let started = r["started_at_ms"].as_i64().unwrap_or(0);
            rows.push((rank(att), mi, started, row));
        }
    }
    rows.sort_by_key(|r| (r.0, r.1, r.2));
    json!({
        "agents": rows.into_iter().map(|r| r.3).collect::<Vec<_>>(),
        "offline": offline,
    })
}

/// Plain-text table for the CLI.
pub fn render_table(v: &Value) -> String {
    let mut out = String::new();
    for a in v["agents"].as_array().into_iter().flatten() {
        let name = a["name"]
            .as_str()
            .or(a["harness"].as_str())
            .unwrap_or("agent");
        out.push_str(&format!(
            "{:<15} {:<22} {:<10} {:<16} {}\n",
            a["attention"].as_str().unwrap_or(""),
            a["qualified"].as_str().unwrap_or(""),
            a["harness"].as_str().unwrap_or(""),
            name,
            a["workspace"].as_str().unwrap_or("")
        ));
    }
    for o in v["offline"].as_array().into_iter().flatten() {
        out.push_str(&format!(
            "offline         {}/                 ({})\n",
            o["machine"].as_str().unwrap_or(""),
            o["error"].as_str().unwrap_or("")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, pane: &str, exec: &str, done: bool, started: i64) -> Value {
        json!({"id": id, "handle": format!("r{id}"), "pane_handle": pane, "harness": "claude",
               "execution": {"value": exec}, "done": done, "open_interactions": 0,
               "started_at_ms": started})
    }

    #[test]
    fn sorted_by_attention_across_machines() {
        let local = MachineAgents {
            machine: "laptop".into(),
            result: Ok((
                json!({"runs": [run("1", "w1:p1", "Working", false, 5),
                                 run("2", "w1:p2", "Idle", true, 1),
                                 run("3", "w1:p3", "Idle", false, 2)]}),
                json!({"interactions": []}),
            )),
        };
        let devbox = MachineAgents {
            machine: "devbox".into(),
            result: Ok((
                json!({"runs": [run("a", "w3:p5", "Working", false, 9),
                                 run("b", "w3:p6", "Error", false, 3),
                                 run("c", "w2:p1", "Working", false, 4)]}),
                json!({"interactions": [
                    {"run": "a", "kind": "approval"},
                    {"run": "c", "kind": "question"}]}),
            )),
        };
        let gpu = MachineAgents {
            machine: "gpu-box".into(),
            result: Err("machine gpu-box did not answer (offline?)".into()),
        };
        let v = combine(vec![local, devbox, gpu]);
        let got: Vec<(String, String)> = v["agents"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| {
                (
                    a["qualified"].as_str().unwrap().to_string(),
                    a["attention"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        assert_eq!(
            got,
            vec![
                ("devbox/w3:p5".into(), "needs_approval".into()),
                ("devbox/w2:p1".into(), "needs_answer".into()),
                ("devbox/w3:p6".into(), "error".into()),
                ("laptop/w1:p2".into(), "done".into()),
                ("laptop/w1:p1".into(), "working".into()),
                ("laptop/w1:p3".into(), "idle".into()),
            ]
        );
        assert_eq!(v["offline"][0]["machine"], "gpu-box");
        let t = render_table(&v);
        assert!(
            t.lines()
                .next()
                .unwrap()
                .starts_with("needs_approval  devbox/w3:p5")
        );
        assert!(t.contains("offline         gpu-box/"));
    }

    #[test]
    fn open_interaction_count_without_details_still_needs_you() {
        let mut r = run("x", "w1:p1", "Working", false, 0);
        r["open_interactions"] = json!(1);
        assert_eq!(attention(&r, &[]), "needs_approval");
        assert_eq!(
            attention(&run("y", "", "RateLimited", false, 0), &[]),
            "error"
        );
        assert_eq!(attention(&run("z", "", "Exited", false, 0), &[]), "exited");
    }
}
