//! Valid-ish seed inputs per target; the property tests mutate these, and `fuzz/` can export
//! them as the starting corpus (`cargo test -p vk-fuzz export_seed_corpus -- --ignored`).

use crate::rng::Rng;
use vk_proto::frame::encode;
use vk_proto::holder::ToHolder;
use vk_proto::render::{ClientFrame, ServerFrame};
use vk_remote::mux::Frame;

fn framed<T: serde::Serialize>(m: &T) -> Vec<u8> {
    encode(m).expect("encode seed")
}

pub fn seeds(target: &str) -> Vec<Vec<u8>> {
    match target {
        "holder_proto_decode" => vec![
            framed(&ToHolder::Hello {
                proto_min: 1,
                proto_max: 1,
                server_pid: 42,
                server_boot_id: "boot".into(),
            }),
            framed(&ToHolder::Acquire {
                epoch: 3,
                server_pid: 42,
                hmac: vec![1, 2, 3],
            }),
            framed(&ToHolder::Attach {
                epoch: 3,
                from_offset: 1 << 40,
            }),
            framed(&ToHolder::Input {
                epoch: 3,
                input_id: 9,
                bytes: b"ls\r".to_vec(),
            }),
        ],
        "render_frame_decode" => vec![
            framed(&ServerFrame::Hello {
                protocol: 1,
                server_version: "0.1.0".into(),
                session: "s".into(),
                machine: "m".into(),
                client_id: "c".into(),
            }),
            framed(&ClientFrame::Ack {
                pane: "p1".into(),
                epoch: 1,
                rev: 7,
            }),
            framed(&ClientFrame::RawInput {
                input_id: 1,
                pane: "p1".into(),
                bytes: b"abc".to_vec(),
            }),
        ],
        "jsonrpc_decode" => vec![
            br#"{"jsonrpc":"2.0","id":1,"method":"pane.list","params":{}}"#.to_vec(),
            br#"{"jsonrpc":"2.0","id":"x\u2028y","method":"pane.send_keys","params":{"keys":["ctrl+c"]}}"#
                .to_vec(),
            br#"{"jsonrpc":"2.0","id":18446744073709551616,"method":"a","params":[[[[[[]]]]]]}"#
                .to_vec(),
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no"}}"#.to_vec(),
        ],
        "key_grammar" => [
            "ctrl+c",
            "shift+enter",
            "alt+P",
            "f12",
            "meta+shift+tab",
            "prefix+v",
            "altgr+q",
            "C-b",
            "ctrl+alt+delete",
            "a..e",
            "1..9",
            "space",
            "minus",
            "+",
            "ctrl++",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect(),
        "vt_parse" => vec![
            b"hello\r\nworld\x1b[31mred\x1b[0m\x1b[2J\x1b[H".to_vec(),
            b"\x1b[?1049h\x1b[10;10Htext\x1b[?1049l".to_vec(),
            b"\x1b]0;title\x07\x1b]52;c;aGk=\x07\x1b]8;;http://x\x1b\\link\x1b]8;;\x1b\\".to_vec(),
            b"\x1b[c\x1b[>c\x1b[6n\x1b[?u\x1b[18t\x1bP+q544e\x1b\\".to_vec(),
            b"\x1b_Gi=1,a=q,s=1,v=1,f=24;AAAA\x1b\\\x1bPq#0;2;0;0;0#0!10~\x1b\\".to_vec(),
            "日本語 e\u{301} 👩\u{200d}👩\u{200d}👧 \x1b[5@\x1b[3P\x1b[2L\x1b[4M".as_bytes().to_vec(),
            b"\x1b[1;1r\x1b[999999999999;5H\x1b[99999999b\x1b[2147483647S".to_vec(),
        ],
        "socks5_handshake" => vec![
            vec![5, 1, 0, 5, 1, 0, 1, 127, 0, 0, 1, 0x1f, 0x90],
            vec![5, 1, 0, 5, 1, 0, 3, 9, b'l', b'o', b'c', b'a', b'l', b'h', b'o', b's', b't', 0, 80],
            vec![5, 2, 0, 2, 5, 1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 80],
            vec![4, 1, 0, 80, 127, 0, 0, 1, 0],
            vec![5, 0],
            vec![5, 1, 0, 5, 1, 0, 3, 0, 0, 80],
        ],
        "mux_frame_decode" => {
            let hello = framed(&Frame::Hello {
                proto: 1,
                version: "0.1.0".into(),
                role: "client".into(),
            });
            let mut full = hello.clone();
            full.extend(framed(&Frame::Open {
                ch: 1,
                kind: "socket".into(),
            }));
            full.extend(framed(&Frame::Data {
                ch: 1,
                bytes: b"payload".to_vec(),
            }));
            full.extend(framed(&Frame::Window { ch: 1, bytes: 1 << 31 }));
            full.extend(framed(&Frame::Close { ch: 1 }));
            full.extend(framed(&Frame::Ping { ts: 5 }));
            vec![hello, full]
        }
        "hook_payloads" => vec![
            br#"{"tool_name":"Bash","tool_input":{"command":"rm -rf /tmp/x"},"tool_use_id":"t1"}"#.to_vec(),
            br#"{"tool_name":"AskUserQuestion","tool_input":{"questions":[{"question":"Which?","header":"h","multiSelect":true,"options":[{"label":"a","description":"b"}]}]}}"#.to_vec(),
            br#"{"tool_name":"Edit","tool_input":{"file_path":"/a","old_string":"x","new_string":"y"}}"#.to_vec(),
            br##"{"tool_name":"ExitPlanMode","tool_input":{"plan":"# p"}}"##.to_vec(),
        ],
        "policy_match" => [
            "http://localhost:3000/x",
            "https://user:pw@[::1]:8443/p?q#f",
            "http://169.254.169.254/latest/meta-data",
            "ws://0x7f.1:80",
            "10.0.0.0/8,db.internal:5432,[::1]:5432,192.168.1.5",
            "http://[::ffff:169.254.169.254]/",
            "::ffff:10.0.0.7/128,::ffff:10.0.0.0/8,[::7]:5432,192.168.1.255.5",
            "file:///etc/passwd",
            "http://a b/",
            "http://:80",
            "http://[",
        ]
        .iter()
        .map(|s| s.as_bytes().to_vec())
        .collect(),
        "kitty_probe" => vec![
            b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;OK\x1b\\\x1b[?2026;2$y\x1b[?62;4;c".to_vec(),
            b"\x1b_Gi=32;ENOTSUPP:no\x1b\\\x1b[<0;10;20M\x1b[<64;1;1m\x1b[?1016;1$y".to_vec(),
            b"\x1b[<0;99999999999;3M\x1b[<;;M\x1b[?\x1b[?;$y\x1b_G".to_vec(),
        ],
        "compat_import" => vec![
            include_bytes!("../../vk-compat/tests/fixtures/herdr-config-full.toml").to_vec(),
            include_bytes!("../../vk-compat/tests/fixtures/herdr-config-real.toml").to_vec(),
            include_bytes!("../../vk-compat/tests/fixtures/herdr-session.json").to_vec(),
            include_bytes!("../../vk-compat/tests/fixtures/herdr-session-splits.json").to_vec(),
        ],
        "manifest_toml" => vec![
            include_bytes!("../../vk-agents/harnesses/claude.toml").to_vec(),
            include_bytes!("../../vk-agents/harnesses/codex.toml").to_vec(),
            include_bytes!("../../vk-agents/harnesses/pi.toml").to_vec(),
            include_bytes!("../../vk-agents/harnesses/hermes.toml").to_vec(),
            include_bytes!("../../vk-agents/harnesses/acp.toml").to_vec(),
            include_bytes!("../../vk-agents/harnesses/examples/espi.toml").to_vec(),
        ],
        "vt_resize_interleave" => vec![
            b"hello world, this line is long enough to wrap when the grid gets narrow\r\nsecond\r\n"
                .to_vec(),
            b"\x1b[?1049h\x1b[5;5Hfixture\x1b[?1049l\x1b[2J\x1b[Hafter".to_vec(),
            "\u{65e5}\u{672c}\u{8a9e} e\u{301} \u{1f469}\u{200d}\u{1f469}\u{200d}\u{1f467} wide and combining\r\n"
                .as_bytes()
                .to_vec(),
            b"\x1b[1;5r\x1b[5;1H\n\n\n\n\nscrolled\x1b[r\x1b[3;7Hcursor".to_vec(),
            b"\x1b[31mred \x1b[1;4;38;2;1;2;3munder\x1b[0m \x1b[8Ghop\x1b[3Ppad\x1b[4@ins".to_vec(),
        ],
        "compat_socket" => vec![
            br#"{"id":"1","method":"workspace.list","params":{}}"#.to_vec(),
            b"{\"id\":\"2\",\"method\":\"pane.send_text\",\"params\":{\"pane_id\":\"p_1\",\"text\":\"ls\"}}\n".to_vec(),
            br#"{"id":"3","method":"events.subscribe","params":{"subscriptions":[{"type":"pane.output_matched","pane_id":"p_1"},{"type":"pane.agent_status_changed","pane_id":"p_1"}]}}"#.to_vec(),
            br#"{"id":"4","method":"ping","vibeke_token":"abc"}"#.to_vec(),
            br#"{"id":4,"method":"x"}"#.to_vec(),
            br#"{"id":"5","method":"","params":[1]}"#.to_vec(),
            b"{\"id\":\"6\",\"method\":\"m\\u2028\",\"params\":null}\r\n{\"id\":\"7\"}".to_vec(),
        ],
        "transcript_parse" => vec![
            br#"{"type":"user","sessionId":"s1","cwd":"/work/repo","timestamp":"2026-10-01T10:00:00.000Z","message":{"role":"user","content":"fix the failing test"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"Looking."},{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}
"#.to_vec(),
            br#"{"type":"session_meta","payload":{"id":"c1","cwd":"/work/repo"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"command\":[\"ls\"]}","call_id":"k1"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"k1","output":"a b"}}
"#.to_vec(),
            {
                // One enormous line: row text must stay bounded.
                let mut v = br#"{"type":"user","message":{"role":"user","content":""#.to_vec();
                v.extend(std::iter::repeat_n(b'x', 200_000));
                v.extend_from_slice(b"\"}}\n");
                v
            },
            b"not json\n{\"type\":\n\xff\xfe\n{\"type\":\"user\"}\npartial".to_vec(),
        ],
        "osc_image" => vec![
            b"i=1,a=T,f=24,s=1,v=1;AAAA".to_vec(),
            b"i=31,s=99999,v=99999,f=32,a=T,t=d;AAAA".to_vec(),
            b"a=T,f=100,m=1;iVBORw0KGgo=".to_vec(),
            b"#0;2;0;0;0#1;2;100;100;100#0!1000~$#1!1000~-#0~~~~".to_vec(),
            b"name=Zm9v;size=3;inline=1:Zm9v".to_vec(),
            b"\x78\x9c\x63\x60\x60\x60\x00\x00\x00\x04\x00\x01".to_vec(),
            "\u{10EEEE}\u{305}\u{305}".as_bytes().to_vec(),
        ],
        _ => vec![],
    }
}

/// One input for `target`: usually a mutated seed, sometimes pure noise.
pub fn case(target: &str, rng: &mut Rng, pool: &[Vec<u8>]) -> Vec<u8> {
    match rng.below(5) {
        0 => rng.bytes(512),
        _ if pool.is_empty() => rng.bytes(512),
        _ => {
            let seed = rng.pick(pool).clone();
            let _ = target;
            rng.mutate(&seed)
        }
    }
}
