//! Property tests (spec 10 §3 `proptest`): layout math, key grammar, redaction and the
//! destination policy. Fast (default 64 cases each; `PROPTEST_CASES` raises it for the nightly
//! run) and deterministic enough to shrink: a failure prints the minimal input.

use proptest::prelude::*;
use vk_proto::layout::{Direction, Rect, rects, split};
use vk_proto::model::LayoutNode;

fn cases() -> ProptestConfig {
    ProptestConfig {
        cases: std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(64),
        // CI prints the minimal input; there is no source-parallel regressions file.
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

// ---- layout math ---------------------------------------------------------------------------

fn leaves(n: &LayoutNode, out: &mut Vec<String>) {
    match n {
        LayoutNode::Leaf { pane } => out.push(pane.clone()),
        LayoutNode::Split { children, .. } => children.iter().for_each(|(c, _)| leaves(c, out)),
    }
}

fn overlap(a: &Rect, b: &Rect) -> bool {
    a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h
}

/// A tree built by `ops` splits: (index of the pane to split, direction, ratio).
fn tree(ops: &[(usize, u8, f32)]) -> LayoutNode {
    let mut t = LayoutNode::Leaf { pane: "p0".into() };
    for (i, (which, d, ratio)) in ops.iter().enumerate() {
        let mut ls = Vec::new();
        leaves(&t, &mut ls);
        let target = ls[which % ls.len()].clone();
        let dir = [
            Direction::Left,
            Direction::Right,
            Direction::Up,
            Direction::Down,
        ][*d as usize % 4];
        assert!(split(&mut t, &target, &format!("p{}", i + 1), dir, *ratio));
    }
    t
}

/// Checks shared by both layout properties; `nonempty` also requires every rect to have area.
fn check_partition(t: &LayoutNode, area: Rect, nonempty: bool) -> Result<(), TestCaseError> {
    let rs = rects(t, area);
    let mut want = Vec::new();
    leaves(t, &mut want);
    let mut got: Vec<String> = rs.iter().map(|(p, _)| p.clone()).collect();
    want.sort();
    got.sort();
    prop_assert_eq!(&got, &want, "one rect per pane");
    for (p, r) in &rs {
        if nonempty {
            prop_assert!(r.w >= 1 && r.h >= 1, "{} has an empty rect {:?}", p, r);
        }
        prop_assert!(
            r.w == 0
                || r.h == 0
                || (r.x >= area.x
                    && r.y >= area.y
                    && r.x + r.w <= area.x + area.w
                    && r.y + r.h <= area.y + area.h),
            "{} {:?} outside {:?}",
            p,
            r,
            area
        );
    }
    for (i, (pa, a)) in rs.iter().enumerate() {
        for (pb, b) in &rs[i + 1..] {
            if a.w > 0 && a.h > 0 && b.w > 0 && b.h > 0 {
                prop_assert!(!overlap(a, b), "{} {:?} overlaps {} {:?}", pa, a, pb, b);
            }
        }
    }
    Ok(())
}

proptest! {
    #![proptest_config(cases())]

    /// Any split history in any area, down to a handful of cells: one rectangle per pane,
    /// inside the area, never overlapping. (Rects may be empty, and then sit anywhere, when the
    /// area is too small to give every pane a cell.)
    #[test]
    fn layout_rects_stay_inside_and_never_overlap(
        ops in proptest::collection::vec((0usize..64, 0u8..4, 0.0f32..1.0), 0..12),
        x in 0u16..50, y in 0u16..50, w in 1u16..200, h in 1u16..80,
    ) {
        check_partition(&tree(&ops), Rect { x, y, w, h }, false)?;
    }

    /// With room to spare (shallow trees, ratios away from the clamp) every pane gets a
    /// non-empty rectangle.
    #[test]
    fn layout_rects_are_non_empty_with_room(
        ops in proptest::collection::vec((0usize..64, 0u8..4, 0.25f32..0.75), 0..5),
        x in 0u16..50, y in 0u16..50,
    ) {
        check_partition(&tree(&ops), Rect { x, y, w: 4000, h: 4000 }, true)?;
    }
}

// ---- key grammar ---------------------------------------------------------------------------

fn key_string() -> impl Strategy<Value = String> {
    let mods =
        proptest::sample::subsequence(vec!["ctrl", "alt", "shift", "super"], 0..=4).prop_shuffle();
    let key = prop_oneof![
        "[a-z0-9]".prop_map(|s| s),
        "[A-Z]".prop_map(|s| s),
        proptest::sample::select(vec![
            "enter",
            "tab",
            "esc",
            "space",
            "backspace",
            "delete",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "insert",
            "f1",
            "f5",
            "f12",
            "minus",
            "comma",
            "period",
            "slash",
        ])
        .prop_map(str::to_string),
    ];
    (mods, key).prop_map(|(m, k)| {
        let mut parts: Vec<String> = m.into_iter().map(str::to_string).collect();
        parts.push(k);
        parts.join("+")
    })
}

proptest! {
    #![proptest_config(cases())]

    /// parse -> print -> parse is stable, and modifier order never changes the meaning.
    #[test]
    fn key_grammar_round_trips(s in key_string()) {
        use vk_term::keygrammar::{format_key, parse_key};
        let ev = parse_key(&s).map_err(|e| TestCaseError::fail(format!("{s:?}: {e}")))?;
        let printed = format_key(&ev);
        let again = parse_key(&printed).map_err(|e| TestCaseError::fail(format!("{printed:?}: {e}")))?;
        prop_assert_eq!(&again, &ev, "{} -> {}", s, printed);
        let mut parts: Vec<&str> = s.split('+').collect();
        let key = parts.pop().unwrap();
        parts.reverse();
        parts.push(key);
        let reordered = parts.join("+");
        prop_assert_eq!(parse_key(&reordered).ok(), Some(ev), "{} vs {}", s, reordered);
    }

    /// Arbitrary text never panics the parsers.
    #[test]
    fn key_grammar_never_panics(s in "\\PC{0,24}") {
        let _ = vk_term::keygrammar::parse_key(&s);
        let _ = vk_term::keygrammar::parse_binding(&s);
        let _ = vk_term::keygrammar::expand_range(&s);
    }
}

// ---- redaction -----------------------------------------------------------------------------

proptest! {
    #![proptest_config(cases())]

    /// Redaction is idempotent: applying it to its own output changes nothing.
    #[test]
    fn redaction_is_idempotent(s in "\\PC{0,200}") {
        let once = vk_redact::redact(&s).into_owned();
        let twice = vk_redact::redact(&once).into_owned();
        prop_assert_eq!(&twice, &once);
    }

    /// A planted credential never survives, wherever it sits in surrounding text.
    #[test]
    fn planted_secrets_are_removed(
        pre in "[a-z .,]{0,30}", post in "[a-z .,]{0,30}",
        body in "[A-Za-z0-9]{40}",
        kind in 0usize..4,
    ) {
        let secret = match kind {
            0 => format!("ghp_{}", &body[..36]),
            1 => format!("sk-ant-{body}"),
            2 => format!("AKIA{}", body[..16].to_uppercase()),
            _ => format!("xoxb-{body}"),
        };
        let out = vk_redact::redact(&format!("{pre} {secret} {post}")).into_owned();
        prop_assert!(!out.contains(&secret), "secret survived: {}", out);
    }

    /// `password=value` style assignments lose the value but keep the key.
    #[test]
    fn assignments_keep_the_key(v in "[A-Za-z0-9]{8,24}", k in proptest::sample::select(vec!["password", "api_key", "client_secret", "token"])) {
        let out = vk_redact::redact(&format!("{k}={v}")).into_owned();
        prop_assert!(out.starts_with(k));
        prop_assert!(!out.contains(&v), "value survived: {}", out);
    }
}

// ---- destination policy --------------------------------------------------------------------

proptest! {
    #![proptest_config(cases())]

    /// With no allow rule, metadata and link-local destinations (IPv4, IPv6 and IPv4-mapped
    /// forms) are never allowed, whatever the port or request kind.
    #[test]
    fn metadata_is_never_allowed_without_a_rule(
        a in any::<[u8; 4]>(), b in any::<[u8; 16]>(), port in any::<u16>(),
        which in 0usize..4, kind in 0usize..3,
    ) {
        use std::net::IpAddr;
        use vk_browser::policy::*;
        let ip: IpAddr = match which {
            0 => IpAddr::from(a),
            1 => IpAddr::from(b),
            2 => IpAddr::V6(std::net::Ipv4Addr::from(a).to_ipv6_mapped()),
            _ => "169.254.169.254".parse().unwrap(),
        };
        let policy = Policy {
            preview_ports: [3000u16].into_iter().collect(),
            foreign_ports: Default::default(),
            allow: vec![],
            external: External::Allow,
        };
        let kind = [Kind::Navigation, Kind::Subresource, Kind::Unknown][kind];
        let d = policy.decide_ip("example.test", ip, port, kind);
        if matches!(classify(canonical(ip)), IpClass::Metadata | IpClass::LinkLocal) {
            prop_assert!(!d.allow, "{ip} allowed");
        }
    }
}
