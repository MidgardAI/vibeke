//! Confirmed task dependency links (15 §8.1, §10.1 `Dependency`, §10.2 `task.dependency.*`, T4).
//!
//! An edge `task → depends_on` says `task` waits for `depends_on`. Only edges a user confirmed
//! exist here (there is no inference); each records who confirmed it. `blocks` edges must stay
//! acyclic and feed the attention ranking as "blocks N tasks"; `related` edges are
//! informational and never rank.

use std::collections::{BTreeSet, HashMap, VecDeque};

use serde::{Deserialize, Serialize};

use crate::{Actor, ActorKind, new_id};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DependencyKind {
    /// `task` cannot finish before `depends_on`.
    #[default]
    Blocks,
    /// Informational link; no ordering, never ranked.
    Related,
}

impl DependencyKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "blocks" | "blocked_by" | "depends_on" => Some(DependencyKind::Blocks),
            "related" | "relates" => Some(DependencyKind::Related),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyEdge {
    pub id: String,
    /// The waiting task.
    pub task: String,
    /// The task it waits for.
    pub depends_on: String,
    pub kind: DependencyKind,
    /// Confirming user (provenance).
    pub confirmed_by: Actor,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DependencyError {
    #[error("a task cannot depend on itself")]
    SelfDependency,
    #[error("this would create a dependency cycle: {}", path.join(" → "))]
    Cycle { path: Vec<String> },
    #[error("this dependency already exists")]
    Duplicate { existing: String },
    #[error("only a user can confirm dependency links")]
    NotUser,
}

/// Validate and build a new confirmed edge against the existing ones (cycle check on `blocks`).
pub fn add_edge(
    existing: &[DependencyEdge],
    task: &str,
    depends_on: &str,
    kind: DependencyKind,
    actor: Actor,
    now_ms: i64,
) -> Result<DependencyEdge, DependencyError> {
    if actor.kind != ActorKind::User {
        return Err(DependencyError::NotUser);
    }
    if task == depends_on {
        return Err(DependencyError::SelfDependency);
    }
    if let Some(e) = existing
        .iter()
        .find(|e| e.task == task && e.depends_on == depends_on && e.kind == kind)
    {
        return Err(DependencyError::Duplicate {
            existing: e.id.clone(),
        });
    }
    if kind == DependencyKind::Blocks
        && let Some(mut path) = blocks_path(existing, depends_on, task)
    {
        // depends_on ⇝ … ⇝ task already; adding task → depends_on closes the loop.
        path.insert(0, task.to_string());
        return Err(DependencyError::Cycle { path });
    }
    Ok(DependencyEdge {
        id: new_id(),
        task: task.to_string(),
        depends_on: depends_on.to_string(),
        kind,
        confirmed_by: actor,
        created_at_ms: now_ms,
    })
}

/// A `blocks` path `from ⇝ to` following "depends on" edges (BFS, shortest).
fn blocks_path(edges: &[DependencyEdge], from: &str, to: &str) -> Option<Vec<String>> {
    let mut prev: HashMap<&str, &str> = HashMap::new();
    let mut q = VecDeque::from([from]);
    let mut seen = BTreeSet::from([from]);
    while let Some(n) = q.pop_front() {
        if n == to {
            let mut path = vec![to.to_string()];
            let mut cur = to;
            while let Some(p) = prev.get(cur) {
                path.push(p.to_string());
                cur = p;
            }
            path.reverse();
            return Some(path);
        }
        for e in edges
            .iter()
            .filter(|e| e.kind == DependencyKind::Blocks && e.task == n)
        {
            if seen.insert(e.depends_on.as_str()) {
                prev.insert(e.depends_on.as_str(), n);
                q.push_back(e.depends_on.as_str());
            }
        }
    }
    None
}

/// For every task, the open tasks that (transitively) wait for it through confirmed `blocks`
/// edges. `is_open` filters out finished/archived dependents (they no longer wait).
pub fn blocked_dependents(
    edges: &[DependencyEdge],
    is_open: impl Fn(&str) -> bool,
) -> HashMap<String, BTreeSet<String>> {
    let blocks: Vec<&DependencyEdge> = edges
        .iter()
        .filter(|e| e.kind == DependencyKind::Blocks)
        .collect();
    let mut out: HashMap<String, BTreeSet<String>> = HashMap::new();
    let roots: BTreeSet<&str> = blocks.iter().map(|e| e.depends_on.as_str()).collect();
    for root in roots {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut q = VecDeque::from([root]);
        while let Some(n) = q.pop_front() {
            for e in blocks.iter().filter(|e| e.depends_on == n) {
                if e.task != root && seen.insert(e.task.clone()) {
                    q.push_back(e.task.as_str());
                }
            }
        }
        seen.retain(|t| is_open(t));
        if !seen.is_empty() {
            out.insert(root.to_string(), seen);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn add(
        v: &mut Vec<DependencyEdge>,
        t: &str,
        d: &str,
    ) -> Result<DependencyEdge, DependencyError> {
        let e = add_edge(v, t, d, DependencyKind::Blocks, Actor::user("alice"), 1)?;
        v.push(e.clone());
        Ok(e)
    }

    #[test]
    fn cycles_self_loops_duplicates_and_agents_are_refused() {
        let mut v = vec![];
        add(&mut v, "b", "a").unwrap(); // b waits for a
        add(&mut v, "c", "b").unwrap(); // c waits for b
        assert_eq!(add(&mut v, "a", "a"), Err(DependencyError::SelfDependency));
        match add(&mut v, "a", "c") {
            Err(DependencyError::Cycle { path }) => assert_eq!(path, ["a", "c", "b", "a"]),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            add(&mut v, "b", "a"),
            Err(DependencyError::Duplicate { .. })
        ));
        // `related` links never form ordering cycles.
        assert!(add_edge(&v, "a", "c", DependencyKind::Related, Actor::user("u"), 1).is_ok());
        assert_eq!(
            add_edge(
                &v,
                "d",
                "a",
                DependencyKind::Blocks,
                Actor::agent("run1"),
                1
            ),
            Err(DependencyError::NotUser)
        );
        let err = add(&mut v, "a", "c").unwrap_err();
        assert!(err.to_string().contains("a → c → b → a"));
    }

    #[test]
    fn blocked_dependents_are_transitive_and_open_only() {
        let mut v = vec![];
        add(&mut v, "b", "a").unwrap();
        add(&mut v, "c", "b").unwrap();
        add(&mut v, "d", "a").unwrap();
        v.push(add_edge(&v, "e", "a", DependencyKind::Related, Actor::user("u"), 1).unwrap());
        let all = blocked_dependents(&v, |_| true);
        assert_eq!(
            all["a"].len(),
            3,
            "b, c (through b) and d; related e excluded"
        );
        assert_eq!(all["b"].len(), 1);
        assert!(!all.contains_key("c"));
        let open = blocked_dependents(&v, |t| t != "d");
        assert_eq!(open["a"].len(), 2);
    }
}
