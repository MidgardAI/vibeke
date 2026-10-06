//! Declarative layout documents (07 §2.14 `LayoutSpec`): what `layout.export` produces and
//! `layout.apply` / `workspace.create {layout}` / `[layouts.<name>]` in config consume.
//! TOML- and JSON-serializable; never crosses the render stream (no postcard constraints).
//!
//! ```toml
//! name = "dev"
//! cwd  = "~/code/app"            # default cwd; relative pane cwds resolve against it
//!
//! [[tab]]
//! title = "edit"
//! focus = true
//! [tab.pane]
//! split = "right"                # right: side by side · down: stacked
//! [[tab.pane.children]]
//! size = 0.6
//! run  = "nvim ."                # typed into the pane's shell after start
//! [[tab.pane.children]]
//! split = "down"
//! children = [{ run = "npm run dev" }, { cwd = "src" }]
//!
//! [[tab.float]]
//! command = "lazygit"            # the pane's process (the pane closes when it exits)
//! rect = { x = 15, y = 15, w = 70, h = 70 }
//! ```

use serde::{Deserialize, Serialize};

/// A whole layout: one or more tabs.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LayoutSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Default cwd (workspace root for a new workspace). `~` expands to `$HOME`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(rename = "tab", alias = "tabs")]
    pub tabs: Vec<TabSpec>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TabSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Relative to the layout cwd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub focus: bool,
    pub pane: PaneSpec,
    #[serde(
        rename = "float",
        alias = "floats",
        skip_serializing_if = "Vec::is_empty"
    )]
    pub floats: Vec<FloatSpec>,
}

/// A layout node: a split (`split` + `children`) or a leaf pane.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PaneSpec {
    /// `right` (children side by side) or `down` (stacked). Aliases: `horizontal`/`h`,
    /// `vertical`/`v`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub split: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<PaneSpec>,
    /// Share of the parent split (any positive numbers; normalised).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<f64>,
    /// The pane's process instead of the default shell.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandSpec>,
    /// A command line typed into the shell once it is up (the shell stays after it exits).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "is_false")]
    pub focus: bool,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FloatSpec {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<CommandSpec>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Percent of the pane area; default 70×70 centred.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rect: Option<RectPct>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct RectPct {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// `"npm run dev"` (run through `/bin/sh -c`) or an argv array.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CommandSpec {
    Shell(String),
    Argv(Vec<String>),
}

impl CommandSpec {
    pub fn argv(&self) -> Vec<String> {
        match self {
            CommandSpec::Shell(s) => vec!["/bin/sh".into(), "-c".into(), s.clone()],
            CommandSpec::Argv(a) => a.clone(),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Split direction of a node: `Some(true)` = side by side, `Some(false)` = stacked, `None` = leaf.
pub fn split_side_by_side(split: &str) -> Option<bool> {
    match split {
        "right" | "left" | "horizontal" | "h" | "row" => Some(true),
        "down" | "up" | "vertical" | "v" | "column" => Some(false),
        _ => None,
    }
}

impl PaneSpec {
    pub fn is_split(&self) -> bool {
        self.split.is_some() || !self.children.is_empty()
    }
    /// Leaves in tree order.
    pub fn leaves(&self) -> Vec<&PaneSpec> {
        let mut v = Vec::new();
        self.collect(&mut v);
        v
    }
    fn collect<'a>(&'a self, v: &mut Vec<&'a PaneSpec>) {
        if self.is_split() {
            self.children.iter().for_each(|c| c.collect(v));
        } else {
            v.push(self);
        }
    }
}

impl LayoutSpec {
    /// Structural checks before anything is spawned. `max_panes` bounds a single apply.
    pub fn validate(&self, max_panes: usize) -> Result<(), String> {
        if self.tabs.is_empty() {
            return Err("layout has no tabs ([[tab]])".into());
        }
        let mut n = 0;
        for (i, t) in self.tabs.iter().enumerate() {
            check_node(&t.pane, &format!("tab[{i}].pane"), 0)?;
            n += t.pane.leaves().len() + t.floats.len();
            for (j, f) in t.floats.iter().enumerate() {
                if let Some(r) = f.rect
                    && !(r.w > 0.0 && r.h > 0.0 && r.x >= 0.0 && r.y >= 0.0)
                {
                    return Err(format!(
                        "tab[{i}].float[{j}].rect must be positive percents"
                    ));
                }
            }
        }
        if n > max_panes {
            return Err(format!("layout has {n} panes (limit {max_panes})"));
        }
        Ok(())
    }
}

fn check_node(p: &PaneSpec, at: &str, depth: usize) -> Result<(), String> {
    if depth > 16 {
        return Err(format!("{at}: nested too deeply"));
    }
    if let Some(s) = p.size
        && !(s > 0.0 && s.is_finite())
    {
        return Err(format!("{at}.size must be positive"));
    }
    if p.is_split() {
        let s = p.split.as_deref().unwrap_or("right");
        if split_side_by_side(s).is_none() {
            return Err(format!("{at}.split must be right|down (got {s})"));
        }
        if p.children.is_empty() {
            return Err(format!("{at}: split without children"));
        }
        if p.command.is_some() || p.run.is_some() {
            return Err(format!("{at}: a split can't have command/run"));
        }
        for (i, c) in p.children.iter().enumerate() {
            check_node(c, &format!("{at}.children[{i}]"), depth + 1)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip_and_validate() {
        let spec: LayoutSpec = serde_json::from_value(serde_json::json!({
            "name": "dev",
            "tab": [{
                "title": "edit",
                "pane": {"split": "right", "children": [
                    {"size": 0.6, "run": "nvim"},
                    {"split": "down", "children": [{"command": ["htop"]}, {"command": "make watch"}]}
                ]},
                "float": [{"command": "lazygit", "rect": {"x": 10, "y": 10, "w": 80, "h": 80}}]
            }]
        }))
        .unwrap();
        spec.validate(64).unwrap();
        assert_eq!(spec.tabs[0].pane.leaves().len(), 3);
        assert_eq!(
            spec.tabs[0].pane.children[1].children[1]
                .command
                .as_ref()
                .unwrap()
                .argv(),
            vec!["/bin/sh", "-c", "make watch"]
        );
        let back: LayoutSpec =
            serde_json::from_value(serde_json::to_value(&spec).unwrap()).unwrap();
        assert_eq!(back, spec);
        assert!(spec.validate(2).is_err());
    }

    #[test]
    fn rejects_bad_docs() {
        let bad: LayoutSpec = serde_json::from_value(serde_json::json!({
            "tab": [{"pane": {"split": "diagonal", "children": [{}]}}]
        }))
        .unwrap();
        assert!(bad.validate(64).unwrap_err().contains("right|down"));
        assert!(LayoutSpec::default().validate(64).is_err());
        assert!(
            serde_json::from_value::<LayoutSpec>(serde_json::json!({"tab": [], "bogus": 1}))
                .is_err()
        );
    }
}
