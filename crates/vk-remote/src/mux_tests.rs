use super::*;

#[tokio::test]
async fn echo_channels_interleave() {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    // Bridge side: each channel is an echo server.
    let acc: Acceptor = Arc::new(|kind: String| {
        Box::pin(async move {
            let (x, y) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(y);
                let _ = w.write_all(format!("hello {kind}\n").as_bytes()).await;
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
            Ok(Box::new(x) as Box<dyn Stream>)
        })
    });
    let _bridge = Mux::start(br, bw, "bridge", Some(acc));
    let client = Mux::start(ar, aw, "client", None);
    let mut c1 = client.open("socket").await.unwrap();
    let c2 = client.open("blob").await.unwrap();
    let mut buf = vec![0u8; 13];
    c1.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"hello socket\n");
    // A large transfer on c2 does not block a small round trip on c1.
    let big = vec![7u8; 2 << 20];
    let big2 = big.clone();
    let (mut r2, mut w2) = tokio::io::split(c2);
    let mut hello = vec![0u8; 11];
    r2.read_exact(&mut hello).await.unwrap();
    let sender = tokio::spawn(async move { w2.write_all(&big2).await.unwrap() });
    c1.write_all(b"ping").await.unwrap();
    let mut p = [0u8; 4];
    tokio::time::timeout(Duration::from_secs(2), c1.read_exact(&mut p))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&p, b"ping");
    let mut got = vec![0u8; big.len()];
    tokio::time::timeout(Duration::from_secs(10), r2.read_exact(&mut got))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got, big);
    sender.await.unwrap();
    assert!(client.stats().bytes_out.load(Ordering::Relaxed) > 2 << 20);
}

async fn send_raw<W: AsyncWrite + Unpin>(w: &mut W, f: &Frame) {
    w.write_all(&vk_proto::frame::encode(f).unwrap())
        .await
        .unwrap();
}

async fn recv_raw<R: AsyncRead + Unpin>(r: &mut R) -> Option<Frame> {
    let body = asyncio::read_body(r).await.ok()?;
    vk_proto::frame::decode(&body).ok()
}

fn sink_acceptor() -> Acceptor {
    // The accepted stream is never read, so the channel's receive path backs up.
    Arc::new(|_k: String| {
        Box::pin(async move {
            let (x, y) = tokio::io::duplex(1024);
            tokio::spawn(async move {
                let _keep = y;
                std::future::pending::<()>().await;
            });
            Ok(Box::new(x) as Box<dyn Stream>)
        })
    })
}

fn echo_acceptor(seen: Arc<Mutex<Vec<String>>>) -> Acceptor {
    Arc::new(move |kind: String| {
        seen.lock().unwrap().push(kind);
        Box::pin(async move {
            let (x, y) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let (mut r, mut w) = tokio::io::split(y);
                let _ = tokio::io::copy(&mut r, &mut w).await;
            });
            Ok(Box::new(x) as Box<dyn Stream>)
        })
    })
}

#[tokio::test]
async fn data_precedes_eof_under_load() {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let (got_tx, got_rx) = oneshot::channel::<Vec<u8>>();
    let got_tx = Arc::new(Mutex::new(Some(got_tx)));
    let acc: Acceptor = Arc::new(move |_k: String| {
        let got_tx = got_tx.clone();
        Box::pin(async move {
            let (x, mut y) = tokio::io::duplex(1 << 16);
            tokio::spawn(async move {
                let mut all = Vec::new();
                let _ = y.read_to_end(&mut all).await; // returns only on EOF
                if let Some(t) = got_tx.lock().unwrap().take() {
                    let _ = t.send(all);
                }
            });
            Ok(Box::new(x) as Box<dyn Stream>)
        })
    });
    let _bridge = Mux::start(br, bw, "bridge", Some(acc));
    let client = Mux::start(ar, aw, "client", None);
    let mut c = client.open("socket").await.unwrap();
    let mut expect = Vec::new();
    for i in 0..400u32 {
        let chunk: Vec<u8> = (0..(1 + (i as usize * 37) % 9000))
            .map(|j| (i as usize + j) as u8)
            .collect();
        c.write_all(&chunk).await.unwrap();
        expect.extend_from_slice(&chunk);
    }
    c.shutdown().await.unwrap(); // EOF right behind the last bytes
    let got = tokio::time::timeout(Duration::from_secs(20), got_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(got.len(), expect.len());
    assert!(got == expect);
}

async fn raw_open<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(r: &mut R, w: &mut W, ch: u32) {
    send_raw(
        w,
        &Frame::Open {
            ch,
            kind: "x".into(),
        },
    )
    .await;
    loop {
        if let Some(Frame::OpenOk { ch: c }) = recv_raw(r).await
            && c == ch
        {
            break;
        }
    }
}

#[tokio::test]
async fn credit_violation_closes_channel() {
    let (a, b) = tokio::io::duplex(8 << 20);
    let (mut hr, mut hw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let _bridge = Mux::start(br, bw, "bridge", Some(sink_acceptor()));
    raw_open(&mut hr, &mut hw, 1).await;
    // Hostile peer: ignore the window and keep sending until the bridge closes the channel.
    let mut sent = 0usize;
    let closed = loop {
        send_raw(
            &mut hw,
            &Frame::Data {
                ch: 1,
                bytes: vec![1; CHUNK],
            },
        )
        .await;
        sent += CHUNK;
        let mut saw_close = false;
        while let Ok(Some(f)) =
            tokio::time::timeout(Duration::from_millis(1), recv_raw(&mut hr)).await
        {
            if f == (Frame::Close { ch: 1 }) {
                saw_close = true;
            }
        }
        if saw_close {
            break true;
        }
        if sent > 16 * WINDOW as usize {
            break false;
        }
    };
    assert!(closed, "bridge never closed the channel");
    // Bytes can only sit in the queue (<= WINDOW) and one duplex hop (<= WINDOW).
    assert!(sent <= 3 * WINDOW as usize + 1024, "accepted {sent} bytes");
    // A single oversized frame is refused immediately on a fresh channel.
    raw_open(&mut hr, &mut hw, 3).await;
    send_raw(
        &mut hw,
        &Frame::Data {
            ch: 3,
            bytes: vec![0; WINDOW as usize + 1],
        },
    )
    .await;
    loop {
        match tokio::time::timeout(Duration::from_secs(5), recv_raw(&mut hr)).await {
            Ok(Some(Frame::Close { ch: 3 })) => break,
            Ok(Some(_)) => {}
            _ => panic!("no Close for oversized frame"),
        }
    }
}

/// Send an old-style `Hello` (no capabilities) from a raw peer.
async fn raw_hello<W: AsyncWrite + Unpin>(w: &mut W, role: &str) {
    send_raw(
        w,
        &Frame::Hello {
            proto: PROTO,
            version: "0.0.1".into(),
            role: role.into(),
        },
    )
    .await;
}

#[tokio::test]
async fn link_drop_wakes_writer_blocked_on_credit() {
    let (a, b) = tokio::io::duplex(8 << 20);
    let (ar, aw) = tokio::io::split(a);
    let (mut hr, mut hw) = tokio::io::split(b);
    let client = Mux::start(ar, aw, "client", None);
    raw_hello(&mut hw, "bridge").await;
    let c2 = client.clone();
    let open = tokio::spawn(async move { c2.open("socket").await });
    let ch = loop {
        if let Some(Frame::Open { ch, .. }) = recv_raw(&mut hr).await {
            break ch;
        }
    };
    send_raw(&mut hw, &Frame::OpenOk { ch }).await;
    let mut c = open.await.unwrap().unwrap();
    // The peer never grants more credit: the writer stalls after WINDOW bytes.
    let writer = tokio::spawn(async move {
        let data = vec![9u8; 4 << 20];
        c.write_all(&data).await
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!writer.is_finished());
    drop(hr);
    drop(hw); // link drops
    let r = tokio::time::timeout(Duration::from_secs(5), writer)
        .await
        .expect("writer still blocked after link drop")
        .unwrap();
    assert!(r.is_err());
    client.closed().await;
    assert!(client.open("socket").await.is_err());
}

#[test]
fn caps_roundtrip_and_old_roles() {
    let r = role_with_caps("client", MuxOpts { compress: true });
    assert!(r.starts_with("client;caps=prio,zstd:"), "{r}");
    assert_eq!(
        parse_caps(&r),
        PeerCaps {
            prio: true,
            zstd: true
        }
    );
    assert_eq!(
        parse_caps(&role_with_caps("bridge", MuxOpts::default())),
        PeerCaps {
            prio: true,
            zstd: false
        }
    );
    // Old peers: bare role; a different dictionary id never enables compression.
    assert_eq!(parse_caps("bridge"), PeerCaps::default());
    assert!(!parse_caps("bridge;caps=prio,zstd:deadbeef").zstd || crate::dict::id() == "deadbeef");
}

#[test]
fn class_hints() {
    assert_eq!(
        split_class("socket#render"),
        ("socket".into(), Class::Render)
    );
    assert_eq!(split_class("socket"), ("socket".into(), Class::Control));
    assert_eq!(
        split_class("tcp:127.0.0.1:80"),
        ("tcp:127.0.0.1:80".into(), Class::Forward)
    );
    // An unknown suffix is part of the kind (the acceptor refuses it).
    assert_eq!(
        split_class("socket#bogus"),
        ("socket#bogus".into(), Class::Control)
    );
    assert_eq!(Class::for_kind("blob"), Class::Blob);
}

fn data(ch: u32, n: u8) -> Frame {
    Frame::Data {
        ch,
        bytes: vec![n; 4],
    }
}

#[test]
fn scheduler_orders_by_class_and_weight() {
    let mut s = Scheduler::default();
    for i in 0..100 {
        s.push(data(9, i), 9, Class::Blob, false);
    }
    for i in 0..3 {
        s.push(data(1, i), 1, Class::Control, false);
    }
    // Control overtakes the queued bulk entirely.
    for _ in 0..3 {
        let (f, _) = s.next().unwrap();
        assert!(matches!(f, Frame::Data { ch: 1, .. }));
    }
    // Render vs blob under contention: 8 render frames per blob frame.
    for i in 0..40 {
        s.push(data(3, i), 3, Class::Render, false);
    }
    let mut order = Vec::new();
    for _ in 0..27 {
        if let Some((Frame::Data { ch, .. }, _)) = s.next() {
            order.push(ch);
        }
    }
    let blobs = order.iter().filter(|c| **c == 9).count();
    let renders = order.iter().filter(|c| **c == 3).count();
    assert!(renders >= 8 * blobs.max(1) - 8 && blobs >= 1, "{order:?}");
    // Alone, blob gets everything.
    while s.next().is_some() {}
    assert!(s.is_empty());
}

#[test]
fn scheduler_keeps_close_behind_data_and_round_robins_a_class() {
    let mut s = Scheduler::default();
    s.push(data(5, 1), 5, Class::Forward, false);
    s.push(Frame::Close { ch: 5 }, 5, Class::Forward, false);
    s.push(data(7, 1), 7, Class::Forward, false);
    s.push(data(7, 2), 7, Class::Forward, false);
    let got: Vec<Frame> = std::iter::from_fn(|| s.next().map(|x| x.0)).collect();
    assert_eq!(
        got,
        vec![data(5, 1), data(7, 1), Frame::Close { ch: 5 }, data(7, 2)]
    );
}

fn pair_with(
    client_opts: MuxOpts,
    bridge_opts: MuxOpts,
    seen: Arc<Mutex<Vec<String>>>,
) -> (Mux, Mux) {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let bridge = Mux::start_with(br, bw, "bridge", Some(echo_acceptor(seen)), bridge_opts);
    let client = Mux::start_with(ar, aw, "client", None, client_opts);
    (client, bridge)
}

fn terminalish(n: usize) -> Vec<u8> {
    let mut v = Vec::new();
    let mut i = 0usize;
    while v.len() < n {
        v.extend_from_slice(
            format!(
                "\x1b[32m{i:>6}\x1b[0m  test remote::mux::tests::case_{} ... ok\r\n",
                i % 97
            )
            .as_bytes(),
        );
        i += 1;
    }
    v.truncate(n);
    v
}

async fn echo_roundtrip(c: &mut DuplexStream, payload: &[u8]) {
    let (mut r, mut w) = tokio::io::split(c);
    let mut got = vec![0u8; payload.len()];
    let (wres, rres) = tokio::join!(w.write_all(payload), r.read_exact(&mut got));
    wres.unwrap();
    rres.unwrap();
    assert!(got == payload, "echo mismatch");
}

#[tokio::test]
async fn render_channel_compresses_when_both_sides_agree() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (client, bridge) = pair_with(
        MuxOpts { compress: true },
        MuxOpts { compress: true },
        seen.clone(),
    );
    let mut c = client.open_class("socket", Class::Render).await.unwrap();
    // The acceptor sees the plain kind; the class travelled as a hint.
    assert_eq!(seen.lock().unwrap().as_slice(), ["socket".to_string()]);
    let payload = terminalish(1 << 20);
    echo_roundtrip(&mut c, &payload).await;
    let cs = client.stats();
    let bs = bridge.stats();
    for (who, s) in [("client", &cs), ("bridge", &bs)] {
        let wire = s.bytes_out.load(Ordering::Relaxed);
        let raw = s.payload_out.load(Ordering::Relaxed);
        assert!(raw >= payload.len() as u64, "{who}: payload {raw}");
        assert!(
            wire * 4 < raw,
            "{who}: {wire} wire bytes for {raw} payload bytes"
        );
        assert!(s.zstd_frames_out.load(Ordering::Relaxed) > 0, "{who}");
    }
    // A control channel on the same link is never compressed.
    let before = cs.zstd_frames_out.load(Ordering::Relaxed);
    let mut ctl = client.open("socket").await.unwrap();
    echo_roundtrip(&mut ctl, &terminalish(64 * 1024)).await;
    assert_eq!(cs.zstd_frames_out.load(Ordering::Relaxed), before);
}

#[tokio::test]
async fn loopback_client_turns_compression_off_both_ways() {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let (client, bridge) = pair_with(MuxOpts::default(), MuxOpts { compress: true }, seen);
    let mut c = client.open_class("socket", Class::Render).await.unwrap();
    echo_roundtrip(&mut c, &terminalish(256 * 1024)).await;
    assert_eq!(client.stats().zstd_frames_out.load(Ordering::Relaxed), 0);
    assert_eq!(bridge.stats().zstd_frames_out.load(Ordering::Relaxed), 0);
    assert_eq!(client.peer_caps().map(|p| p.zstd), Some(true));
    assert_eq!(bridge.peer_caps().map(|p| p.zstd), Some(false));
}

#[tokio::test]
async fn old_peer_gets_plain_kinds_and_plain_data() {
    let (a, b) = tokio::io::duplex(1 << 20);
    let (ar, aw) = tokio::io::split(a);
    let (mut hr, mut hw) = tokio::io::split(b);
    let client = Mux::start_with(ar, aw, "client", None, MuxOpts { compress: true });
    raw_hello(&mut hw, "bridge").await;
    let c2 = client.clone();
    let open = tokio::spawn(async move { c2.open_class("socket", Class::Render).await });
    let (ch, kind) = loop {
        if let Some(Frame::Open { ch, kind }) = recv_raw(&mut hr).await {
            break (ch, kind);
        }
    };
    assert_eq!(kind, "socket", "no class suffix for a peer without `prio`");
    send_raw(&mut hw, &Frame::OpenOk { ch }).await;
    let mut c = open.await.unwrap().unwrap();
    c.write_all(&terminalish(8192)).await.unwrap();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), recv_raw(&mut hr)).await {
            Ok(Some(Frame::Data { ch: c2, bytes })) if c2 == ch => {
                assert!(!bytes.is_empty());
                break;
            }
            Ok(Some(Frame::DataZ { .. })) => panic!("compressed data to an old peer"),
            Ok(Some(_)) => {}
            _ => panic!("no data"),
        }
    }
}

#[tokio::test]
async fn undecodable_compressed_frame_closes_the_channel() {
    let (a, b) = tokio::io::duplex(8 << 20);
    let (mut hr, mut hw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let _bridge = Mux::start_with(
        br,
        bw,
        "bridge",
        Some(sink_acceptor()),
        MuxOpts { compress: true },
    );
    raw_open(&mut hr, &mut hw, 1).await;
    raw_open(&mut hr, &mut hw, 3).await;
    send_raw(
        &mut hw,
        &Frame::DataZ {
            ch: 1,
            bytes: vec![0x28, 0xb5, 0x2f, 0xfd, 1, 2, 3],
        },
    )
    .await;
    // A zstd bomb (decompresses past one chunk) is refused the same way.
    let bomb = zstd::bulk::Compressor::with_dictionary(3, crate::dict::bytes())
        .unwrap()
        .compress(&vec![0u8; 4 * CHUNK])
        .unwrap();
    send_raw(&mut hw, &Frame::DataZ { ch: 3, bytes: bomb }).await;
    let mut closed = Vec::new();
    while closed.len() < 2 {
        match tokio::time::timeout(Duration::from_secs(5), recv_raw(&mut hr)).await {
            Ok(Some(Frame::Close { ch })) => closed.push(ch),
            Ok(Some(_)) => {}
            _ => panic!("channels not closed: {closed:?}"),
        }
    }
    closed.sort();
    assert_eq!(closed, vec![1, 3]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn keystrokes_overtake_a_saturated_blob_channel() {
    // A slow link (small pipe): the blob channel keeps the writer busy; small control writes
    // must still come back quickly.
    let (a, b) = tokio::io::duplex(32 * 1024);
    let (ar, aw) = tokio::io::split(a);
    let (br, bw) = tokio::io::split(b);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let _bridge = Mux::start(br, bw, "bridge", Some(echo_acceptor(seen)));
    let client = Mux::start(ar, aw, "client", None);
    let mut ctl = client.open("socket").await.unwrap();
    let blob = client.open_class("socket", Class::Blob).await.unwrap();
    let (mut br2, mut bw2) = tokio::io::split(blob);
    let drain = tokio::spawn(async move {
        let mut sink = vec![0u8; 1 << 16];
        while br2.read(&mut sink).await.unwrap_or(0) > 0 {}
    });
    let bulk = tokio::spawn(async move {
        let chunk = vec![1u8; 1 << 16];
        // Until aborted: the link stays saturated for the whole measurement.
        while bw2.write_all(&chunk).await.is_ok() {}
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let t = Instant::now();
    for i in 0..20u8 {
        ctl.write_all(&[i]).await.unwrap();
        let mut b = [0u8; 1];
        tokio::time::timeout(Duration::from_secs(10), ctl.read_exact(&mut b))
            .await
            .expect("keystroke stuck behind bulk")
            .unwrap();
        assert_eq!(b[0], i);
    }
    // Generous for loaded hosts: a mux without priorities leaves keystrokes stuck behind an
    // ever-growing blob queue, which the per-keystroke timeout above still catches.
    assert!(t.elapsed() < Duration::from_secs(15), "{:?}", t.elapsed());
    assert!(!bulk.is_finished(), "bulk kept the link busy throughout");
    bulk.abort();
    drain.abort();
}

#[tokio::test]
async fn close_stays_behind_queued_data_after_the_channel_is_forgotten() {
    // A slow link (1 KiB pipe the peer does not read yet) backs a blob channel up in the
    // scheduler. The peer half-closes first, so once the local side finishes, the channel
    // leaves the channel map while its data is still queued. Its remaining Data and the
    // Close must keep the blob class: filed under another class they would overtake the
    // queued data, reordering the stream and delivering EOF early.
    let (a, b) = tokio::io::duplex(1024);
    let (ar, aw) = tokio::io::split(a);
    let (mut hr, mut hw) = tokio::io::split(b);
    let client = Mux::start(ar, aw, "client", None);
    raw_hello(&mut hw, "bridge").await;
    let c2 = client.clone();
    let open = tokio::spawn(async move { c2.open_class("socket", Class::Blob).await });
    let ch = loop {
        if let Some(Frame::Open { ch, .. }) = recv_raw(&mut hr).await {
            break ch;
        }
    };
    send_raw(&mut hw, &Frame::OpenOk { ch }).await;
    let mut c = open.await.unwrap().unwrap();
    send_raw(&mut hw, &Frame::Close { ch }).await; // peer half-closes
    // Stay under the credit window, so the sender reaches EOF without a Window grant.
    let expect: Vec<u8> = (0..WINDOW as usize - 1024)
        .map(|i| (i % 251) as u8)
        .collect();
    let half = expect.len() / 2;
    // First half: the writer files it under the blob class and stalls on the pipe.
    c.write_all(&expect[..half]).await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    // Second half and EOF: both directions are now closed, so the channel is forgotten.
    c.write_all(&expect[half..]).await.unwrap();
    c.shutdown().await.unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !client.shared.chans.lock().unwrap().contains_key(&ch),
        "channel should be forgotten while its frames are still queued"
    );
    // Now drain the link: every byte, in order, then Close.
    let mut got = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(5), recv_raw(&mut hr)).await {
            Ok(Some(Frame::Data { ch: c, bytes })) if c == ch => got.extend_from_slice(&bytes),
            Ok(Some(Frame::Close { ch: c })) if c == ch => break,
            Ok(Some(_)) => {}
            other => panic!("link ended before Close: {other:?}"),
        }
    }
    assert_eq!(got.len(), expect.len(), "EOF arrived before all data");
    assert!(got == expect, "data reordered");
}
