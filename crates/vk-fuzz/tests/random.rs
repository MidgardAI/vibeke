//! Bounded random property run over every fuzz target on stable (spec 10 §6). Deterministic:
//! `VK_FUZZ_SEED` picks the stream, `VK_FUZZ_CASES` the per-target count (default 300; the
//! engine and mux targets run a tenth of that, they are slower).

use vk_fuzz::{TARGETS, rng::Rng, seeds};

fn env_u64(k: &str, d: u64) -> u64 {
    std::env::var(k)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(d)
}

fn run_target(name: &str) {
    let t = TARGETS.iter().find(|t| t.name == name).expect("target");
    let mut cases = env_u64("VK_FUZZ_CASES", 300) as usize;
    if matches!(name, "vt_parse" | "mux_frame_decode") {
        cases = (cases / 10).max(10);
    }
    let pool = seeds::seeds(name);
    // Seeds themselves must be accepted.
    for s in &pool {
        (t.run)(s);
    }
    let mut rng = Rng::new(env_u64("VK_FUZZ_SEED", 0x5eed) ^ name.len() as u64);
    for i in 0..cases {
        let input = seeds::case(name, &mut rng, &pool);
        let r = std::panic::catch_unwind(|| (t.run)(&input));
        if r.is_err() {
            panic!("target {name} panicked on case {i}; input = {input:?}");
        }
    }
}

macro_rules! target_tests {
    ($($n:ident),*) => { $( #[test] fn $n() { run_target(stringify!($n)); } )* };
}

target_tests!(
    holder_proto_decode,
    render_frame_decode,
    jsonrpc_decode,
    key_grammar,
    vt_parse,
    socks5_handshake,
    mux_frame_decode,
    hook_payloads,
    policy_match,
    kitty_probe,
    compat_import,
    manifest_toml
);

#[test]
fn every_target_has_a_test_and_seeds() {
    assert_eq!(TARGETS.len(), 12);
    for t in TARGETS {
        assert!(!seeds::seeds(t.name).is_empty(), "{} has no seeds", t.name);
    }
}

/// `cargo test -p vk-fuzz export_seed_corpus -- --ignored` writes fuzz/corpus/<target>/seed-N.
#[test]
#[ignore]
fn export_seed_corpus() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fuzz/corpus");
    for t in TARGETS {
        let dir = root.join(t.name);
        std::fs::create_dir_all(&dir).unwrap();
        for (i, s) in seeds::seeds(t.name).iter().enumerate() {
            std::fs::write(dir.join(format!("seed-{i}")), s).unwrap();
        }
    }
}
