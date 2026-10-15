//! Layout tree operations shared by server (PTY sizing, focus moves) and clients (drawing).
//! Splits are separated by a one-cell border.

use crate::model::{LayoutNode, SplitDir};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn contains(&self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

impl Direction {
    pub fn parse(s: &str) -> Option<Direction> {
        Some(match s {
            "left" | "h" => Direction::Left,
            "right" | "l" => Direction::Right,
            "up" | "k" => Direction::Up,
            "down" | "j" => Direction::Down,
            _ => return None,
        })
    }
}

/// Insert `new` next to `target`. Returns false if `target` isn't in the tree.
pub fn split(tree: &mut LayoutNode, target: &str, new: &str, dir: Direction, ratio: f32) -> bool {
    let ratio = ratio.clamp(0.05, 0.95);
    let sdir = match dir {
        Direction::Left | Direction::Right => SplitDir::Horizontal,
        Direction::Up | Direction::Down => SplitDir::Vertical,
    };
    let new_first = matches!(dir, Direction::Left | Direction::Up);
    split_rec(tree, target, new, sdir, new_first, ratio)
}

fn split_rec(
    node: &mut LayoutNode,
    target: &str,
    new: &str,
    sdir: SplitDir,
    new_first: bool,
    ratio: f32,
) -> bool {
    match node {
        LayoutNode::Leaf { pane } if pane == target => {
            let old = LayoutNode::Leaf { pane: pane.clone() };
            let fresh = LayoutNode::Leaf {
                pane: new.to_string(),
            };
            // `ratio` is the share of the new pane.
            let children = if new_first {
                vec![(fresh, ratio), (old, 1.0 - ratio)]
            } else {
                vec![(old, 1.0 - ratio), (fresh, ratio)]
            };
            *node = LayoutNode::Split {
                dir: sdir,
                children,
            };
            true
        }
        LayoutNode::Leaf { .. } => false,
        LayoutNode::Split { dir, children } => {
            // Same-direction split: insert as a sibling instead of nesting.
            if *dir == sdir
                && let Some(i) = children
                    .iter()
                    .position(|(c, _)| matches!(c, LayoutNode::Leaf { pane } if pane == target))
            {
                let share = children[i].1;
                let new_share = share * ratio;
                children[i].1 = share - new_share;
                let leaf = LayoutNode::Leaf {
                    pane: new.to_string(),
                };
                let at = if new_first { i } else { i + 1 };
                children.insert(at, (leaf, new_share));
                return true;
            }
            children
                .iter_mut()
                .any(|(c, _)| split_rec(c, target, new, sdir, new_first, ratio))
        }
    }
}

/// Remove a pane; returns the remaining tree or None if it was the last pane.
pub fn remove(tree: &LayoutNode, pane: &str) -> Option<LayoutNode> {
    match tree {
        LayoutNode::Leaf { pane: p } => (p != pane).then(|| tree.clone()),
        LayoutNode::Split { dir, children } => {
            let kept: Vec<(LayoutNode, f32)> = children
                .iter()
                .filter_map(|(c, r)| remove(c, pane).map(|n| (n, *r)))
                .collect();
            match kept.len() {
                0 => None,
                1 => Some(kept.into_iter().next().unwrap().0),
                _ => {
                    let sum: f32 = kept.iter().map(|(_, r)| r).sum();
                    Some(LayoutNode::Split {
                        dir: *dir,
                        children: kept.into_iter().map(|(c, r)| (c, r / sum)).collect(),
                    })
                }
            }
        }
    }
}

/// Pane rectangles within `area`.
pub fn rects(tree: &LayoutNode, area: Rect) -> Vec<(String, Rect)> {
    let mut out = Vec::new();
    rects_rec(tree, area, &mut out);
    out
}

fn rects_rec(node: &LayoutNode, a: Rect, out: &mut Vec<(String, Rect)>) {
    match node {
        LayoutNode::Leaf { pane } => out.push((pane.clone(), a)),
        LayoutNode::Split { dir, children } => {
            let n = children.len() as u16;
            let total = match dir {
                SplitDir::Horizontal => a.w,
                SplitDir::Vertical => a.h,
            };
            let avail = total.saturating_sub(n.saturating_sub(1));
            let sum: f32 = children.iter().map(|(_, r)| r).sum::<f32>().max(0.0001);
            let mut pos = 0u16;
            let mut acc = 0f32;
            for (i, (child, r)) in children.iter().enumerate() {
                acc += r / sum;
                let end = if i + 1 == children.len() {
                    avail
                } else {
                    ((acc * avail as f32).round() as u16).min(avail)
                };
                let start = pos;
                let len = end.saturating_sub(start).max(1);
                let off = start + i as u16; // borders
                let sub = match dir {
                    SplitDir::Horizontal => Rect {
                        x: a.x + off,
                        y: a.y,
                        w: len.min(a.w.saturating_sub(off)),
                        h: a.h,
                    },
                    SplitDir::Vertical => Rect {
                        x: a.x,
                        y: a.y + off,
                        w: a.w,
                        h: len.min(a.h.saturating_sub(off)),
                    },
                };
                rects_rec(child, sub, out);
                pos = end;
            }
        }
    }
}

/// The pane in `dir` from `from`, choosing the one with the largest overlap.
pub fn neighbor(rects: &[(String, Rect)], from: &str, dir: Direction) -> Option<String> {
    let (_, f) = rects.iter().find(|(p, _)| p == from)?;
    let mut best: Option<(i32, String)> = None;
    for (p, r) in rects {
        if p == from {
            continue;
        }
        let (adjacent, overlap) = match dir {
            Direction::Left => (
                r.x + r.w < f.x + 1 && r.x + r.w + 2 >= f.x,
                ov(r.y, r.h, f.y, f.h),
            ),
            Direction::Right => (
                r.x > f.x + f.w.saturating_sub(1) && r.x <= f.x + f.w + 1,
                ov(r.y, r.h, f.y, f.h),
            ),
            Direction::Up => (
                r.y + r.h < f.y + 1 && r.y + r.h + 2 >= f.y,
                ov(r.x, r.w, f.x, f.w),
            ),
            Direction::Down => (
                r.y > f.y + f.h.saturating_sub(1) && r.y <= f.y + f.h + 1,
                ov(r.x, r.w, f.x, f.w),
            ),
        };
        if adjacent && overlap > 0 && best.as_ref().is_none_or(|(o, _)| overlap > *o) {
            best = Some((overlap, p.clone()));
        }
    }
    best.map(|(_, p)| p)
}

fn ov(a: u16, al: u16, b: u16, bl: u16) -> i32 {
    let s = a.max(b) as i32;
    let e = (a + al).min(b + bl) as i32;
    e - s
}

/// Grow (`delta` > 0) or shrink the pane toward `dir` by changing the nearest enclosing split.
pub fn resize(tree: &mut LayoutNode, pane: &str, dir: Direction, delta: f32) -> bool {
    let want = match dir {
        Direction::Left | Direction::Right => SplitDir::Horizontal,
        Direction::Up | Direction::Down => SplitDir::Vertical,
    };
    resize_rec(tree, pane, want, dir, delta)
}

fn resize_rec(
    node: &mut LayoutNode,
    pane: &str,
    want: SplitDir,
    dir: Direction,
    delta: f32,
) -> bool {
    let LayoutNode::Split { dir: d, children } = node else {
        return false;
    };
    let Some(i) = children.iter().position(|(c, _)| c.contains(pane)) else {
        return false;
    };
    if resize_rec(&mut children[i].0, pane, want, dir, delta) {
        return true;
    }
    if *d != want {
        return false;
    }
    let forward = matches!(dir, Direction::Right | Direction::Down);
    let j = if forward { i + 1 } else { i.wrapping_sub(1) };
    if j >= children.len() {
        return false;
    }
    let d = delta.min(children[j].1 - 0.05).max(-(children[i].1 - 0.05));
    children[i].1 += d;
    children[j].1 -= d;
    true
}

/// Change width (left shrinks, right grows) or height (up shrinks, down grows).
/// Prefer the following sibling, falling back to the previous one at the outer edge.
/// Unlike `resize`, direction describes the size change, not the neighbour to take from.
pub fn resize_size(tree: &mut LayoutNode, pane: &str, dir: Direction, amount: f32) -> bool {
    let horizontal = matches!(dir, Direction::Left | Direction::Right);
    let grow = matches!(dir, Direction::Right | Direction::Down);
    let want = if horizontal {
        SplitDir::Horizontal
    } else {
        SplitDir::Vertical
    };
    fn visit(node: &mut LayoutNode, pane: &str, want: SplitDir, delta: f32) -> bool {
        let LayoutNode::Split { dir, children } = node else {
            return false;
        };
        let Some(i) = children.iter().position(|(c, _)| c.contains(pane)) else {
            return false;
        };
        if visit(&mut children[i].0, pane, want, delta) {
            return true;
        }
        if *dir != want || children.len() < 2 {
            return false;
        }
        let j = if i + 1 < children.len() { i + 1 } else { i - 1 };
        // Do not make an already-small child smaller, or grow a small neighbour as a
        // side effect of clamping. Shares can be below 5% after repeated splits.
        let d = delta.clamp(
            -(children[i].1 - 0.05).max(0.0),
            (children[j].1 - 0.05).max(0.0),
        );
        children[i].1 += d;
        children[j].1 -= d;
        true
    }
    visit(tree, pane, want, if grow { amount } else { -amount })
}

/// Apply a size change without shrinking any terminal below four columns or two
/// rows in the caller's viewport. If the viewport is already smaller, do not make
/// that pane smaller still. Bisect the step rather than refusing a large step outright.
pub fn resize_size_in(tree: &mut LayoutNode, pane: &str, dir: Direction, amount: f32, area: Rect) {
    let before = rects(tree, area);
    let original = tree.clone();
    let acceptable = |candidate: &LayoutNode| {
        rects(candidate, area).iter().all(|(id, r)| {
            let old = before.iter().find(|(p, _)| p == id).unwrap().1;
            r.w >= old.w.min(4) && r.h >= old.h.min(2)
        })
    };
    resize_size(tree, pane, dir, amount);
    if acceptable(tree) {
        return;
    }
    let (mut low, mut high) = (0.0, amount);
    *tree = original.clone();
    for _ in 0..20 {
        let mid = (low + high) / 2.0;
        let mut candidate = original.clone();
        resize_size(&mut candidate, pane, dir, mid);
        if acceptable(&candidate) {
            low = mid;
            *tree = candidate;
        } else {
            high = mid;
        }
    }
}

/// Toggle only the immediate split containing this leaf. Preserve order and ratios.
pub fn toggle_split(node: &mut LayoutNode, pane: &str) -> bool {
    let LayoutNode::Split { dir, children } = node else {
        return false;
    };
    if children
        .iter()
        .any(|(c, _)| matches!(c, LayoutNode::Leaf { pane: p } if p == pane))
    {
        *dir = match dir {
            SplitDir::Horizontal => SplitDir::Vertical,
            SplitDir::Vertical => SplitDir::Horizontal,
        };
        return true;
    }
    children.iter_mut().any(|(c, _)| toggle_split(c, pane))
}

/// Exchange the leaves, keeping the shape and size of each position.
pub fn swap(node: &mut LayoutNode, a: &str, b: &str) {
    match node {
        LayoutNode::Leaf { pane } if pane == a => *pane = b.into(),
        LayoutNode::Leaf { pane } if pane == b => *pane = a.into(),
        LayoutNode::Split { children, .. } => {
            for (c, _) in children {
                swap(c, a, b);
            }
        }
        _ => {}
    }
}

pub const PRESETS: &[&str] = &["columns", "rows", "main-left", "main-top", "tiled"];

/// Arrange existing leaves. This never creates or replaces terminals.
pub fn preset(panes: &[String], name: &str) -> Option<LayoutNode> {
    if panes.is_empty() || !PRESETS.contains(&name) {
        return None;
    }
    fn line(nodes: Vec<LayoutNode>, dir: SplitDir) -> LayoutNode {
        if nodes.len() == 1 {
            return nodes.into_iter().next().unwrap();
        }
        let share = 1.0 / nodes.len() as f32;
        LayoutNode::Split {
            dir,
            children: nodes.into_iter().map(|n| (n, share)).collect(),
        }
    }
    let leaves: Vec<_> = panes
        .iter()
        .map(|p| LayoutNode::Leaf { pane: p.clone() })
        .collect();
    if leaves.len() == 1 {
        return Some(leaves.into_iter().next().unwrap());
    }
    Some(match name {
        "columns" => line(leaves, SplitDir::Horizontal),
        "rows" => line(leaves, SplitDir::Vertical),
        "main-left" | "main-top" => {
            let mut rest = leaves.into_iter();
            let main = rest.next().unwrap();
            let (outer, inner) = if name == "main-left" {
                (SplitDir::Horizontal, SplitDir::Vertical)
            } else {
                (SplitDir::Vertical, SplitDir::Horizontal)
            };
            LayoutNode::Split {
                dir: outer,
                children: vec![(main, 0.6), (line(rest.collect(), inner), 0.4)],
            }
        }
        "tiled" => {
            let cols = (leaves.len() as f64).sqrt().ceil() as usize;
            let rows = leaves
                .chunks(cols)
                .map(|row| line(row.to_vec(), SplitDir::Horizontal))
                .collect();
            line(rows, SplitDir::Vertical)
        }
        _ => unreachable!(),
    })
}

/// Identify a preset from its tree, so cycling needs no extra persisted state.
pub fn next_preset(tree: &LayoutNode) -> &'static str {
    let panes = tree.panes();
    let i = PRESETS
        .iter()
        .position(|name| preset(&panes, name).is_some_and(|t| &t == tree));
    PRESETS[i.map_or(0, |i| (i + 1) % PRESETS.len())]
}

/// Make every split's children equal.
pub fn equalize(node: &mut LayoutNode) {
    if let LayoutNode::Split { children, .. } = node {
        let n = children.len() as f32;
        for (c, r) in children.iter_mut() {
            *r = 1.0 / n;
            equalize(c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(p: &str) -> LayoutNode {
        LayoutNode::Leaf { pane: p.into() }
    }

    #[test]
    fn split_remove_rects_neighbors() {
        let mut t = leaf("a");
        assert!(split(&mut t, "a", "b", Direction::Right, 0.5));
        assert!(split(&mut t, "b", "c", Direction::Down, 0.5));
        let rs = rects(
            &t,
            Rect {
                x: 0,
                y: 0,
                w: 81,
                h: 25,
            },
        );
        assert_eq!(rs.len(), 3);
        let a = rs.iter().find(|(p, _)| p == "a").unwrap().1;
        let b = rs.iter().find(|(p, _)| p == "b").unwrap().1;
        let c = rs.iter().find(|(p, _)| p == "c").unwrap().1;
        assert_eq!(
            a,
            Rect {
                x: 0,
                y: 0,
                w: 40,
                h: 25
            }
        );
        assert_eq!(b.x, 41);
        assert_eq!(b.w, 40);
        assert_eq!(b.h + c.h + 1, 25);
        assert_eq!(neighbor(&rs, "a", Direction::Right).as_deref(), Some("b"));
        assert_eq!(neighbor(&rs, "b", Direction::Down).as_deref(), Some("c"));
        assert_eq!(neighbor(&rs, "c", Direction::Left).as_deref(), Some("a"));
        assert_eq!(neighbor(&rs, "a", Direction::Left), None);
        assert!(resize(&mut t, "a", Direction::Right, 0.1));
        let t2 = remove(&t, "b").unwrap();
        assert_eq!(t2.panes(), vec!["a", "c"]);
        assert!(remove(&leaf("a"), "a").is_none());
    }

    #[test]
    fn same_direction_split_is_flat() {
        let mut t = leaf("a");
        split(&mut t, "a", "b", Direction::Right, 0.5);
        split(&mut t, "b", "c", Direction::Right, 0.5);
        let LayoutNode::Split { children, .. } = &t else {
            panic!()
        };
        assert_eq!(children.len(), 3);
    }
}

#[cfg(test)]
mod pane_control_tests {
    use super::*;

    fn pair(dir: Direction) -> LayoutNode {
        let mut t = LayoutNode::Leaf { pane: "a".into() };
        split(&mut t, "a", "b", dir, 0.5);
        t
    }
    fn share(t: &LayoutNode, index: usize) -> f32 {
        let LayoutNode::Split { children, .. } = t else {
            panic!()
        };
        children[index].1
    }
    #[test]
    fn both_panes_can_shrink_and_grow_on_both_axes() {
        for (axis, shrink, grow) in [
            (Direction::Right, Direction::Left, Direction::Right),
            (Direction::Down, Direction::Up, Direction::Down),
        ] {
            for (i, pane) in ["a", "b"].iter().enumerate() {
                let mut t = pair(axis);
                assert!(resize_size(&mut t, pane, shrink, 0.1));
                assert!((share(&t, i) - 0.4).abs() < 0.0001);
                assert!(resize_size(&mut t, pane, grow, 0.1));
                assert!((share(&t, i) - 0.5).abs() < 0.0001);
                for _ in 0..30 {
                    resize_size(&mut t, pane, shrink, 0.25);
                }
                assert!((share(&t, i) - 0.05).abs() < 0.0001);
                for _ in 0..30 {
                    resize_size(&mut t, pane, grow, 0.25);
                }
                assert!((share(&t, i) - 0.95).abs() < 0.0001);
            }
        }
    }
    #[test]
    fn nested_resize_and_toggle_leave_unrelated_split_unchanged() {
        let mut t = pair(Direction::Right);
        split(&mut t, "b", "c", Direction::Down, 0.5);
        assert!(resize_size(&mut t, "c", Direction::Up, 0.1));
        assert_eq!(share(&t, 0), 0.5);
        let LayoutNode::Split { children, .. } = &t else {
            panic!()
        };
        assert!((share(&children[1].0, 1) - 0.4).abs() < 0.0001);
        let before = serde_json::to_value(&t).unwrap();
        assert!(toggle_split(&mut t, "c"));
        let LayoutNode::Split { dir, children } = &t else {
            panic!()
        };
        assert_eq!(*dir, SplitDir::Horizontal);
        assert!(matches!(
            children[1].0,
            LayoutNode::Split {
                dir: SplitDir::Horizontal,
                ..
            }
        ));
        assert!(toggle_split(&mut t, "c"));
        assert_eq!(serde_json::to_value(&t).unwrap(), before);
    }
    #[test]
    fn presets_and_swaps_keep_every_pane_exactly_once() {
        for n in 1..15 {
            let panes: Vec<String> = (0..n).map(|i| format!("p{i}")).collect();
            for name in PRESETS {
                let mut t = preset(&panes, name).unwrap();
                assert_eq!(t.panes(), panes);
                let before = serde_json::to_value(&t).unwrap();
                swap(&mut t, &panes[0], &panes[n - 1]);
                assert_eq!(t.panes()[0], panes[n - 1]);
                swap(&mut t, &panes[0], &panes[n - 1]);
                assert_eq!(serde_json::to_value(&t).unwrap(), before);
                let rs = rects(
                    &t,
                    Rect {
                        x: 0,
                        y: 0,
                        w: 160,
                        h: 50,
                    },
                );
                assert!(rs.iter().all(|(_, r)| r.w > 0 && r.h > 0));
            }
        }
    }
    #[test]
    fn viewport_limits_protect_nested_panes_and_allow_partial_steps() {
        let mut t = pair(Direction::Right);
        split(&mut t, "b", "c", Direction::Down, 0.5);
        let area = Rect {
            x: 0,
            y: 0,
            w: 17,
            h: 7,
        };
        for _ in 0..20 {
            resize_size_in(&mut t, "a", Direction::Right, 0.25, area);
            resize_size_in(&mut t, "c", Direction::Up, 0.25, area);
        }
        let rs = rects(&t, area);
        assert!(rs.iter().all(|(_, r)| r.w >= 4 && r.h >= 2), "{rs:?}");
        assert!(rs[0].1.w > 8, "a large step should still move to the limit");
        // A tiny viewport must not panic or reverse the requested direction.
        let tiny = Rect {
            x: 0,
            y: 0,
            w: 2,
            h: 2,
        };
        let before = rects(&t, tiny);
        resize_size_in(&mut t, "c", Direction::Left, 0.25, tiny);
        let after = rects(&t, tiny);
        assert!(
            before
                .iter()
                .zip(&after)
                .all(|((_, a), (_, b))| b.w >= a.w.min(4) && b.h >= a.h.min(2))
        );
    }

    #[test]
    fn cycles_all_five_distinct_presets() {
        let panes = vec!["a".into(), "b".into(), "c".into(), "d".into()];
        let mut t = preset(&panes, "columns").unwrap();
        for name in ["rows", "main-left", "main-top", "tiled", "columns"] {
            assert_eq!(next_preset(&t), name);
            t = preset(&panes, name).unwrap();
        }
    }
}
