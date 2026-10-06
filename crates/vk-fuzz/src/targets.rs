//! The targets. Oracles are "no panic" plus the cheap invariants noted per target.

use crate::rng::Rng;
use std::net::IpAddr;
use std::time::Duration;

pub struct Target {
    pub name: &'static str,
    pub run: fn(&[u8]),
}

pub const TARGETS: &[Target] = &[
    Target {
        name: "holder_proto_decode",
        run: holder_proto_decode,
    },
    Target {
        name: "render_frame_decode",
        run: render_frame_decode,
    },
    Target {
        name: "jsonrpc_decode",
        run: jsonrpc_decode,
    },
    Target {
        name: "key_grammar",
        run: key_grammar,
    },
    Target {
        name: "vt_parse",
        run: vt_parse,
    },
    Target {
        name: "socks5_handshake",
        run: socks5_handshake,
    },
    Target {
        name: "mux_frame_decode",
        run: mux_frame_decode,
    },
    Target {
        name: "hook_payloads",
        run: hook_payloads,
    },
    Target {
        name: "policy_match",
        run: policy_match,
    },
    Target {
        name: "kitty_probe",
        run: kitty_probe,
    },
    Target {
        name: "compat_import",
        run: compat_import,
    },
    Target {
        name: "manifest_toml",
        run: manifest_toml,
    },
];

fn lossy(data: &[u8]) -> String {
    String::from_utf8_lossy(data).into_owned()
}

/// Feed `data` as a byte stream through `FrameBuf` in irregular chunks, decoding each body as
/// `T`; also decode the raw bytes directly.
fn framebuf_drain<T: serde::de::DeserializeOwned>(data: &[u8]) {
    let mut rng = Rng::from_bytes(data);
    let mut fb = vk_proto::frame::FrameBuf::default();
    let mut rest = data;
    while !rest.is_empty() {
        let n = 1 + rng.below(rest.len().min(64));
        fb.push(&rest[..n]);
        rest = &rest[n..];
        loop {
            match fb.next_frame::<T>() {
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(vk_proto::frame::FrameError::Decode(_)) => {}
                Err(_) => return, // oversized: the caller would drop the connection
            }
        }
    }
    let _ = vk_proto::frame::decode::<T>(data);
}

pub fn holder_proto_decode(data: &[u8]) {
    framebuf_drain::<vk_proto::holder::ToHolder>(data);
    framebuf_drain::<vk_proto::holder::FromHolder>(data);
}

pub fn render_frame_decode(data: &[u8]) {
    framebuf_drain::<vk_proto::render::ServerFrame>(data);
    framebuf_drain::<vk_proto::render::ClientFrame>(data);
}

pub fn jsonrpc_decode(data: &[u8]) {
    use vk_proto::rpc::{Notification, Request, Response};
    let _ = serde_json::from_slice::<Request>(data);
    let _ = serde_json::from_slice::<Response>(data);
    let _ = serde_json::from_slice::<Notification>(data);
    // Control API lines: one JSON document per line (U+2028/2029 are not separators).
    for line in lossy(data).split('\n') {
        let _ = serde_json::from_str::<Request>(line);
        let _ = serde_json::from_str::<serde_json::Value>(line);
    }
}

pub fn key_grammar(data: &[u8]) {
    use vk_term::keygrammar::{format_key, parse_binding, parse_key};
    let s = lossy(data);
    if let Ok(ev) = parse_key(&s) {
        // print -> parse must be stable.
        let printed = format_key(&ev);
        let again = parse_key(&printed)
            .unwrap_or_else(|e| panic!("format_key({ev:?}) = {printed:?} does not parse: {e}"));
        assert_eq!(again, ev, "round-trip via {printed:?}");
    }
    let _ = parse_binding(&s);
    let _ = vk_term::keygrammar::expand_range(&s);
}

/// Random bytes (and resize operations derived from them) into the VT engine.
pub fn vt_parse(data: &[u8]) {
    use vk_term::Engine;
    let started = std::time::Instant::now();
    let mut rng = Rng::from_bytes(data);
    let cols = 2 + rng.below(200) as u16;
    let rows = 1 + rng.below(80) as u16;
    let mut e = Engine::new(cols, rows, 1000);
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let n = 1 + rng.below(rest.len().min(256));
        e.feed(&rest[..n], &mut out);
        rest = &rest[n..];
        if rng.chance(8) {
            e.resize(1 + rng.below(200) as u16, 1 + rng.below(80) as u16);
        }
        if out.len() > 10_000 {
            out.clear();
        }
    }
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "engine too slow on {} bytes",
        data.len()
    );
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

pub fn socks5_handshake(data: &[u8]) {
    use vk_preview::socks;
    let data = data.to_vec();
    rt().block_on(async move {
        // The reader is the fuzz input (then EOF); replies go to a sink.
        let mut io = tokio::io::join(std::io::Cursor::new(data), tokio::io::sink());
        if let Ok(dest) = socks::handshake(&mut io).await {
            let _ = dest.host();
            let _ = dest.is_loopback();
            let _ = dest.channel_target();
        }
    });
}

pub fn mux_frame_decode(data: &[u8]) {
    use vk_remote::mux::{Frame, Mux};
    framebuf_drain::<Frame>(data);
    let data = data.to_vec();
    rt().block_on(async move {
        // A whole mux fed hostile frames: it must close or ignore, never panic or hang.
        let acceptor: vk_remote::mux::Acceptor = std::sync::Arc::new(|_kind| {
            Box::pin(async {
                let (a, _b) = tokio::io::duplex(4096);
                Ok(Box::new(a) as Box<dyn vk_remote::mux::Stream>)
            })
        });
        let mux = Mux::start(
            std::io::Cursor::new(data),
            tokio::io::sink(),
            "bridge",
            Some(acceptor),
        );
        for _ in 0..20 {
            if mux.is_closed() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    });
}

const HOOK_KEYS: &[&str] = &[
    "tool_name",
    "tool_input",
    "tool_use_id",
    "call_id",
    "turn_id",
    "questions",
    "question",
    "header",
    "multiSelect",
    "options",
    "label",
    "description",
    "plan",
    "command",
    "file_path",
    "path",
    "old_string",
    "new_string",
    "content",
    "url",
    "pattern",
    "AskUserQuestion",
    "ExitPlanMode",
    "Bash",
    "Edit",
    "Write",
    "WebFetch",
    "Task",
    "message",
    "method",
    "title",
    "kind",
    "options",
    "optionId",
    "toolCall",
    "rawInput",
];
const HOOK_EVENTS: &[&str] = &[
    "PermissionRequest",
    "PreToolUse",
    "Dialog",
    "permission.ask",
    "RequestPermission",
    "Notification",
    "",
];

/// Arbitrary JSON payloads into the hook -> Interaction mapping.
pub fn hook_payloads(data: &[u8]) {
    use vk_server::agents::harness::{Harness, interaction_from_hook};
    let mut rng = Rng::from_bytes(data);
    let harnesses = Harness::all();
    let payload = match serde_json::from_slice::<serde_json::Value>(data) {
        Ok(v) if rng.chance(2) => v,
        _ => rng.json(4, HOOK_KEYS),
    };
    let h = *rng.pick(&harnesses);
    let event = if rng.chance(8) {
        rng.string(&[])
    } else {
        (*rng.pick(HOOK_EVENTS)).to_string()
    };
    if let Some(it) = interaction_from_hook(h, &event, &payload) {
        // Whatever the payload, the decision for the interaction must be buildable.
        let _ = serde_json::to_string(&it);
    }
}

/// URL / destination policy. Oracle: never allow metadata/link-local addresses without a rule
/// naming them, never panic on odd URLs.
pub fn policy_match(data: &[u8]) {
    use vk_browser::policy::*;
    let mut rng = Rng::from_bytes(data);
    let s = lossy(data);
    let _ = parse_target(&s);
    let _ = split_host_port(&s, 80);
    let _ = is_local_scheme(&s);
    let _ = is_localhost_name(&s);
    let _ = External::parse(&s);
    let rules: Vec<AllowRule> = s.split([',', ' ']).filter_map(AllowRule::parse).collect();
    let policy = Policy {
        preview_ports: [3000u16, 5173].into_iter().collect(),
        foreign_ports: Default::default(),
        allow: if rng.chance(2) { rules } else { vec![] },
        external: *rng.pick(&[External::Deny, External::Subresources, External::Allow]),
    };
    const INTERESTING_IPS: &[&str] = &[
        "169.254.169.254",
        "169.254.170.2",
        "100.100.100.200",
        "fd00:ec2::254",
        "::ffff:169.254.169.254",
        "::ffff:127.0.0.1",
        "127.0.0.1",
        "::1",
        "10.0.0.1",
        "fe80::1",
        "0.0.0.0",
        "8.8.8.8",
    ];
    let ip: IpAddr = match rng.below(3) {
        0 => IpAddr::from([
            rng.next_u64() as u8,
            rng.next_u64() as u8,
            rng.next_u64() as u8,
            rng.next_u64() as u8,
        ]),
        1 => {
            let mut b = [0u8; 16];
            for x in &mut b {
                *x = rng.next_u64() as u8;
            }
            IpAddr::from(b)
        }
        _ => rng.pick(INTERESTING_IPS).parse().expect("literal"),
    };
    let kind = *rng.pick(&[Kind::Navigation, Kind::Subresource, Kind::Unknown]);
    let port = rng.next_u64() as u16;
    let _ = classify(ip);
    let _ = canonical(ip);
    let d = policy.decide_ip(&s, ip, port, kind);
    if matches!(
        classify(canonical(ip)),
        IpClass::Metadata | IpClass::LinkLocal
    ) && policy.allow.is_empty()
    {
        assert!(!d.allow, "metadata/link-local {ip} allowed without a rule");
    }
    let _ = policy.decide(&s, port, &[ip], kind);
    let _ = policy.decide(&s, port, &[], kind);
    if let Some(t) = parse_target(&s) {
        let _ = policy.decide(&t.host, t.port, &[ip], kind);
    }
}

/// Terminal replies and mouse reports are attacker-influenced (a program can print anything).
pub fn kitty_probe(data: &[u8]) {
    use vk_browser::probe::*;
    let replies = parse_replies(data);
    let _ = GraphicsCaps::from_replies(&replies);
    if let Some((_, used)) = parse_sgr_mouse(data) {
        assert!(used <= data.len());
    }
}

pub fn compat_import(data: &[u8]) {
    let s = lossy(data);
    let _ = vk_compat::import_config(&s);
    let _ = vk_compat::import_session(&s);
}

pub fn manifest_toml(data: &[u8]) {
    use vk_agents::manifest::*;
    let s = lossy(data);
    if let Ok(raw) = parse_raw(&s, Source::Builtin) {
        let mut table = raw.table.clone();
        let _ = sanitize_remote(&mut table);
        if let Ok(m) = raw.table.clone().try_into::<Manifest>()
            && let Ok(l) = Loaded::new(m, Source::Builtin, "claude".into())
        {
            {
                let _ = l.evaluate("Do you want to proceed?\n❯ 1. Yes\n  2. No");
                let _ = l.matches_process(&["node".into(), "x.js".into()], Some("/usr/bin/node"));
                let _ = l.parse_version("tool v1.2.3 (build)");
                let _ = l.validated("1.2.3");
                let _ = l.capabilities(Some("1.2.3"), "interactive");
                let _ = l.resume_argv("abc");
            }
        }
    }
    let _ = VersionReq::parse(&s);
    let _ = parse_version_token(&s);
    let _ = version_triple(&s);
}
