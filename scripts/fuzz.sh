#!/bin/sh
# Optional libFuzzer run of the targets in fuzz/ (spec 10 §6). Needs a nightly toolchain and
# cargo-fuzz; neither is installed by mise.toml, so this is never part of `mise run ci`.
#
#   FUZZ_TIME=300 mise run fuzz              # 300 s per target (default 60)
#   FUZZ_TARGET=policy_match mise run fuzz   # one target
#   FUZZ_SANITIZER=none mise run fuzz        # skip ASan (needed for the native vt engine on some hosts)
set -eu
if ! rustup toolchain list 2>/dev/null | grep -q '^nightly'; then
  echo "fuzz: no nightly toolchain (rustup toolchain install nightly); the stable fallback is 'mise run fuzz-smoke'" >&2
  exit 0
fi
if ! cargo +nightly fuzz --help >/dev/null 2>&1; then
  echo "fuzz: cargo-fuzz not installed (cargo +nightly install cargo-fuzz); stable fallback: 'mise run fuzz-smoke'" >&2
  exit 0
fi
TIME="${FUZZ_TIME:-60}"
SAN="${FUZZ_SANITIZER:-address}"
TARGETS="${FUZZ_TARGET:-$(ls fuzz/fuzz_targets | sed 's/\.rs$//')}"
status=0
for t in $TARGETS; do
  echo "== $t (${TIME}s)"
  # Corpus lives in fuzz/corpus/<target> (checked in seeds + whatever the run finds).
  cargo +nightly fuzz run --fuzz-dir fuzz --sanitizer "$SAN" "$t" fuzz/corpus/"$t" -- \
    -max_total_time="$TIME" -rss_limit_mb=2048 -max_len=65536 || status=$?
done
exit $status
