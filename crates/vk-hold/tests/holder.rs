use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use vk_hold::holder::hmac;
use vk_proto::frame::{read_frame, write_frame};
use vk_proto::holder::*;

struct Srv {
    s: UnixStream,
    epoch: u64,
}

fn spec(dir: &Path, argv: &[&str]) -> SpawnSpec {
    SpawnSpec {
        pane_id: "p1".into(),
        socket: dir.join("h.sock").to_string_lossy().into(),
        argv: argv.iter().map(|s| s.to_string()).collect(),
        cwd: dir.to_string_lossy().into(),
        env: vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("TERM".into(), "xterm-256color".into()),
        ],
        key: vec![7; 32],
        cols: 80,
        rows: 24,
        ring_bytes: 1 << 20,
    }
}

fn launch(dir: &Path, argv: &[&str]) -> (SpawnSpec, vk_hold::Launched) {
    let sp = spec(dir, argv);
    let l = vk_hold::launch(
        Path::new(env!("CARGO_BIN_EXE_vk-hold")),
        &[],
        &sp,
        dir,
        Some(&dir.join("holder.log")),
    )
    .unwrap();
    (sp, l)
}

fn connect(sp: &SpawnSpec, epoch: u64) -> Srv {
    let mut s = UnixStream::connect(&sp.socket).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    write_frame(
        &mut s,
        &ToHolder::Hello {
            proto_min: 1,
            proto_max: 1,
            server_pid: 1,
            server_boot_id: "t".into(),
        },
    )
    .unwrap();
    let FromHolder::HelloOk { nonce, .. } = read_frame(&mut s).unwrap() else {
        panic!()
    };
    write_frame(
        &mut s,
        &ToHolder::Acquire {
            epoch,
            server_pid: 1,
            hmac: hmac(&sp.key, &nonce, epoch),
        },
    )
    .unwrap();
    let m: FromHolder = read_frame(&mut s).unwrap();
    assert_eq!(m, FromHolder::Acquired { epoch });
    Srv { s, epoch }
}

impl Srv {
    fn attach(&mut self, from: u64) -> (Vec<u8>, Vec<MarkerKind>) {
        write_frame(
            &mut self.s,
            &ToHolder::Attach {
                epoch: self.epoch,
                from_offset: from,
            },
        )
        .unwrap();
        let mut out = vec![];
        let mut marks = vec![];
        loop {
            match read_frame(&mut self.s).unwrap() {
                FromHolder::Output { bytes, replay, .. } => {
                    assert!(replay);
                    out.extend(bytes)
                }
                FromHolder::Marker { kind, .. } => marks.push(kind),
                FromHolder::ReplayDone { .. } => return (out, marks),
                FromHolder::Gap { .. } => {}
                other => panic!("unexpected {other:?}"),
            }
        }
    }
    fn read_until(&mut self, needle: &str, acc: &mut Vec<u8>) -> Vec<FromHolder> {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut others = vec![];
        while !String::from_utf8_lossy(acc).contains(needle) {
            assert!(
                Instant::now() < deadline,
                "timeout waiting for {needle:?}; got {:?}",
                String::from_utf8_lossy(acc)
            );
            match read_frame(&mut self.s).unwrap() {
                FromHolder::Output { bytes, .. } => acc.extend(bytes),
                o => others.push(o),
            }
        }
        others
    }
}

fn tmp() -> (tempfile::TempDir, PathBuf) {
    let d = tempfile::Builder::new()
        .prefix("vkh")
        .tempdir_in("/tmp")
        .unwrap();
    let p = d.path().to_path_buf();
    (d, p)
}

#[test]
fn survives_server_loss_dedupes_input_and_fences() {
    let (_d, dir) = tmp();
    let (sp, l) = launch(
        &dir,
        &[
            "/bin/sh",
            "-c",
            "echo hello-holder; stty -echo; while read x; do echo got:$x; done",
        ],
    );
    let mut a = connect(&sp, 1);
    let (replayed, marks) = a.attach(0);
    assert!(matches!(
        marks[0],
        MarkerKind::Resize {
            cols: 80,
            rows: 24,
            ..
        }
    ));
    let mut acc = replayed;
    a.read_until("hello-holder", &mut acc);
    // let stty run
    std::thread::sleep(Duration::from_millis(200));
    write_frame(
        &mut a.s,
        &ToHolder::Input {
            epoch: 1,
            input_id: 42,
            bytes: b"one\n".to_vec(),
        },
    )
    .unwrap();
    let others = a.read_until("got:one", &mut acc);
    // Duplicate id is not written again.
    write_frame(
        &mut a.s,
        &ToHolder::Input {
            epoch: 1,
            input_id: 42,
            bytes: b"one\n".to_vec(),
        },
    )
    .unwrap();
    let mut acks: Vec<InputStatus> = others
        .iter()
        .filter_map(|m| {
            if let FromHolder::InputAck { status, .. } = m {
                Some(*status)
            } else {
                None
            }
        })
        .collect();
    while acks.len() < 2 {
        if let FromHolder::InputAck { status, .. } = read_frame(&mut a.s).unwrap() {
            acks.push(status);
        }
    }
    assert_eq!(acks, vec![InputStatus::Written, InputStatus::Duplicate]);

    // "Server crash": drop the connection. The child keeps running.
    drop(a);
    assert_eq!(
        unsafe { libc::kill(l.child_pid as i32, 0) },
        0,
        "child alive"
    );

    // New server, higher epoch; replay has everything.
    let mut b = connect(&sp, 2);
    let (replayed, marks) = b.attach(0);
    let text = String::from_utf8_lossy(&replayed);
    assert!(
        text.contains("hello-holder") && text.contains("got:one"),
        "{text}"
    );
    assert!(
        marks
            .iter()
            .any(|m| matches!(m, MarkerKind::InputWritten { input_id: 42 }))
    );
    assert!(
        marks
            .iter()
            .any(|m| matches!(m, MarkerKind::ServerDetached))
    );

    // A stale server (epoch 1) cannot acquire.
    let mut s = UnixStream::connect(&sp.socket).unwrap();
    write_frame(
        &mut s,
        &ToHolder::Hello {
            proto_min: 1,
            proto_max: 1,
            server_pid: 1,
            server_boot_id: "t".into(),
        },
    )
    .unwrap();
    let FromHolder::HelloOk { nonce, .. } = read_frame(&mut s).unwrap() else {
        panic!()
    };
    write_frame(
        &mut s,
        &ToHolder::Acquire {
            epoch: 1,
            server_pid: 1,
            hmac: hmac(&sp.key, &nonce, 1),
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame::<_, FromHolder>(&mut s).unwrap(),
        FromHolder::Rejected { .. }
    ));
    // A wrong key cannot acquire.
    write_frame(
        &mut s,
        &ToHolder::Acquire {
            epoch: 9,
            server_pid: 1,
            hmac: hmac(&[1; 32], &nonce, 9),
        },
    )
    .unwrap();
    assert!(matches!(
        read_frame::<_, FromHolder>(&mut s).unwrap(),
        FromHolder::Rejected { .. }
    ));

    // Dedupe survives the server change (same input id from a retrying client).
    write_frame(
        &mut b.s,
        &ToHolder::Input {
            epoch: 2,
            input_id: 42,
            bytes: b"one\n".to_vec(),
        },
    )
    .unwrap();
    loop {
        if let FromHolder::InputAck { status, .. } = read_frame(&mut b.s).unwrap() {
            assert_eq!(status, InputStatus::Duplicate);
            break;
        }
    }

    // Child exit is reported; AckExit lets the holder exit.
    unsafe { libc::kill(l.child_pid as i32, libc::SIGKILL) };
    loop {
        if let FromHolder::ChildExited { signal, .. } = read_frame(&mut b.s).unwrap() {
            assert_eq!(signal, Some(libc::SIGKILL));
            break;
        }
    }
    write_frame(&mut b.s, &ToHolder::AckExit { epoch: 2 }).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(l.holder_pid as i32, 0) } == 0 {
        assert!(Instant::now() < deadline, "holder did not exit");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn answers_identification_queries_without_server() {
    let (_d, dir) = tmp();
    // The child asks DA1 and prints the reply in hex once it arrives.
    let script = "stty raw -echo; printf '\\033[c'; dd bs=1 count=12 2>/dev/null | od -An -c | tr -d ' \\n'; echo; sleep 5";
    let (sp, _l) = launch(&dir, &["/bin/sh", "-c", script]);
    std::thread::sleep(Duration::from_millis(500));
    let mut a = connect(&sp, 1);
    let (replayed, _) = a.attach(0);
    let mut acc = replayed;
    a.read_until("62;22;52c", &mut acc);
}

#[test]
fn resize_is_journaled() {
    let (_d, dir) = tmp();
    let (sp, _l) = launch(&dir, &["/bin/sh", "-c", "sleep 0.5; stty size; sleep 5"]);
    let mut a = connect(&sp, 1);
    a.attach(0);
    write_frame(
        &mut a.s,
        &ToHolder::Resize {
            epoch: 1,
            cols: 100,
            rows: 30,
            px_w: 0,
            px_h: 0,
        },
    )
    .unwrap();
    let mut acc = vec![];
    let others = a.read_until("30 100", &mut acc);
    assert!(others.iter().any(|m| matches!(
        m,
        FromHolder::Marker {
            kind: MarkerKind::Resize {
                cols: 100,
                rows: 30,
                ..
            },
            ..
        }
    )));
}
