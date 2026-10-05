//! C4 hard gate (03 §2.3): serialize/restore must round-trip the full state, including parser
//! state, across cuts in the middle of escape and UTF-8 sequences. Cut every 4,093rd byte,
//! snapshot, restore into a fresh engine, continue; the result must equal the uninterrupted run.

use std::path::PathBuf;
use vk_term::Engine;

fn corpus() -> Vec<(String, Vec<u8>)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/vt-corpus");
    let mut v: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "raw"))
        .map(|e| {
            (
                e.file_name().to_string_lossy().into_owned(),
                std::fs::read(e.path()).unwrap(),
            )
        })
        .collect();
    v.sort();
    v.push(("synthetic".into(), synthetic()));
    v
}

/// Sequences designed to straddle cut points: long SGR runs, OSC titles, UTF-8 of every
/// length, alt screen, scroll regions, kitty keyboard pushes, DECSC/DECRC, charsets, tabs.
fn synthetic() -> Vec<u8> {
    let mut s = Vec::new();
    for i in 0..400u32 {
        s.extend(
            format!(
                "\x1b[38;2;{};{};{}m",
                i % 256,
                (i * 7) % 256,
                (i * 13) % 256
            )
            .as_bytes(),
        );
        s.extend("línea ünïcödé 日本 🎉 ✔️ ".as_bytes());
        s.extend(format!("\x1b]0;title {i}\x07").as_bytes());
        if i % 50 == 0 {
            s.extend(b"\x1b[?1049h\x1b[2J\x1b[H alt \x1b7\x1b[10;5Hsaved\x1b8 back");
        }
        if i % 50 == 25 {
            s.extend(b"\x1b[?1049l");
        }
        if i % 30 == 0 {
            s.extend(b"\x1b[3;12r\x1b[12;1H\n\n\x1b[r");
        }
        if i % 40 == 0 {
            s.extend(b"\x1b[>5u\x1b(0lqqk\x1b(B\x1b[3g\x1bH\t|");
        }
        if i % 70 == 0 {
            s.extend(b"\x1b[<u\x1b[?2004h\x1b[?1000h\x1b[?1006h");
        }
        s.extend(b"\x1b[0m\r\n");
    }
    s
}

fn state(e: &Engine) -> String {
    let mut out = format!(
        "{:?}\n{:?}\n{}\n{}\n",
        e.cursor(),
        e.modes(),
        e.term_mode(),
        e.title()
    );
    for i in 0..e.history_len() {
        out.push_str(&format!("{:?}\n", e.history_row(i).unwrap()));
    }
    for r in e.visible_rows() {
        out.push_str(&format!("{r:?}\n"));
    }
    out
}

#[test]
fn c4_snapshot_restore_at_every_4093rd_byte() {
    for (name, data) in corpus() {
        let (cols, rows) = if name.starts_with("top") {
            (120, 40)
        } else {
            (100, 30)
        };
        let mut reference = Engine::new(cols, rows, 2000);
        let mut fx = vec![];
        reference.feed(&data, &mut fx);

        let mut e = Engine::new(cols, rows, 2000);
        let mut mid_seq_cuts = 0;
        for chunk in data.chunks(4093) {
            e.feed(chunk, &mut fx);
            let snap = e.snapshot();
            let restored = Engine::restore(&snap, 2000).expect("restore");
            if !e.tracker_pending_is_empty() {
                mid_seq_cuts += 1;
            }
            e = restored;
        }
        assert_eq!(
            state(&e),
            state(&reference),
            "corpus {name} diverged after restore"
        );
        eprintln!(
            "{name}: {} bytes, {} cuts, {mid_seq_cuts} mid-sequence",
            data.len(),
            data.len().div_ceil(4093)
        );
    }
}

#[test]
fn cut_inside_every_sequence_kind() {
    let data = synthetic();
    // Exhaustive: cut at every byte offset of the first 3 KiB.
    let mut reference = Engine::new(80, 24, 500);
    let mut fx = vec![];
    reference.feed(&data[..3072], &mut fx);
    for cut in 1..3072 {
        let mut e = Engine::new(80, 24, 500);
        e.feed(&data[..cut], &mut fx);
        let mut r = Engine::restore(&e.snapshot(), 500).unwrap();
        r.feed(&data[cut..3072], &mut fx);
        assert_eq!(state(&r), state(&reference), "diverged at cut {cut}");
    }
}

#[test]
fn replay_suppresses_side_effects() {
    let mut e = Engine::new(80, 24, 100);
    e.set_replaying(true);
    let mut fx = vec![];
    e.feed(
        b"\x07\x1b]9;hello\x07\x1b]52;c;aGk=\x07\x1b[c\x1b[6n\x1b]0;t\x07",
        &mut fx,
    );
    assert!(
        fx.iter()
            .all(|f| matches!(f, vk_term::Effect::TitleChanged | vk_term::Effect::Cwd(_))),
        "{fx:?}"
    );
    e.set_replaying(false);
    e.feed(b"\x1b[c", &mut fx);
    assert!(fx.contains(&vk_term::Effect::Reply(
        vk_proto::ident::DA1.as_bytes().to_vec()
    )));
}

#[test]
fn chunked_utf8_split_matches_unbroken() {
    let data = "ab ünïc".as_bytes();
    let mut a = Engine::new(20, 2, 0);
    let mut fx = vec![];
    a.feed(data, &mut fx);
    for cut in 1..data.len() {
        let mut b = Engine::new(20, 2, 0);
        b.feed(&data[..cut], &mut fx);
        b.feed(&data[cut..], &mut fx);
        assert_eq!(b.screen_text(), a.screen_text(), "cut {cut}");
    }
}

/// C5 throughput (03 §2.1): run with `cargo test --release -p vk-term -- --ignored --nocapture`.
#[test]
#[ignore]
fn throughput() {
    let mut data = Vec::new();
    for (_, d) in corpus() {
        data.extend(d);
    }
    let mut e = Engine::new(120, 40, 10_000);
    let mut fx = vec![];
    let total = 200usize << 20;
    let start = std::time::Instant::now();
    let mut fed = 0;
    while fed < total {
        for c in data.chunks(65536) {
            e.feed(c, &mut fx);
            fx.clear();
            let _ = e.take_damage();
        }
        fed += data.len();
    }
    let secs = start.elapsed().as_secs_f64();
    eprintln!(
        "throughput: {:.0} MB/s over {} MB",
        fed as f64 / secs / 1e6,
        fed >> 20
    );
    let t = std::time::Instant::now();
    let snap = e.snapshot();
    eprintln!(
        "snapshot {} KiB in {:?}; restore {:?}",
        snap.len() / 1024,
        t.elapsed(),
        {
            let t = std::time::Instant::now();
            let _ = Engine::restore(&snap, 10_000).unwrap();
            t.elapsed()
        }
    );
}
