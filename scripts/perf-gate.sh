#!/bin/sh
# Relative performance gate (spec 10 section 2.2): runs the startup/recovery timing tests and
# the VT throughput test several times in release mode, normalizes every number by a machine
# calibration taken in the same sample, and compares the median ratios with
# tests/perf/baseline.json (scripts/perf_compare.py). Safe on noisy shared runners: it only
# fails when the median is worse than the baseline by more than PERF_TOLERANCE (default 1.0 =
# 2x), ignores samples that disagree by more than 3x, and only reports when the baseline has
# no entry for this platform.
#
#   sh scripts/perf-gate.sh                    # gate against tests/perf/baseline.json
#   PERF_UPDATE=1 sh scripts/perf-gate.sh      # re-record this platform's baseline (quiet machine)
#
# Environment: PERF_SAMPLES (5), PERF_TOLERANCE (1.0), PERF_OUT (target/perf-gate),
# PERF_BASELINE (tests/perf/baseline.json). A candidate baseline for this platform is always
# written to $PERF_OUT/candidate.json so CI can publish it as an artifact.
set -eu
cd "$(dirname "$0")/.."
SAMPLES="${PERF_SAMPLES:-5}"
OUT="${PERF_OUT:-target/perf-gate}"
BASELINE="${PERF_BASELINE:-tests/perf/baseline.json}"
rm -rf "$OUT"
mkdir -p "$OUT"
# Absolute: cargo runs test binaries from the package directory.
OUT_ABS=$(cd "$OUT" && pwd)

cargo test --release -p vibeke --test timing --no-run
cargo test --release -p vk-term --test recovery --no-run

i=1
while [ "$i" -le "$SAMPLES" ]; do
  d="$OUT/s$i"
  mkdir -p "$d"
  python3 scripts/perf_compare.py calibrate >"$d/cal.json"
  # One test thread: the timing tests must not compete with each other for the machine.
  VIBEKE_TIMING_REPORT="$OUT_ABS/s$i/timing.tsv" cargo test --release -p vibeke --test timing -- \
    --test-threads=1 >"$d/timing.log" 2>&1 || { echo "perf: timing tests failed in sample $i"; tail -30 "$d/timing.log"; exit 1; }
  if cargo test --release -p vk-term --test recovery throughput -- --ignored --nocapture \
      >"$d/vt.log" 2>&1; then
    sed -n 's/.*throughput: \([0-9]*\) MB\/s.*/\1/p' "$d/vt.log" | head -1 >"$d/vt.txt"
  fi
  python3 scripts/perf_compare.py calibrate >"$d/cal2.json"
  # Keep the calmer of the two calibrations (before/after): the machine was busiest when
  # the later one is slower, and the lower number is the better estimate of its real speed.
  python3 - "$d" <<'EOF'
import json, sys
d = sys.argv[1]
a, b = json.load(open(f"{d}/cal.json")), json.load(open(f"{d}/cal2.json"))
json.dump({k: min(a[k], b[k]) for k in a}, open(f"{d}/cal.json", "w"))
EOF
  echo "perf: sample $i/$SAMPLES done"
  i=$((i + 1))
done

set -- "$OUT" --baseline "$BASELINE" --candidate "$OUT/candidate.json"
[ -n "${PERF_TOLERANCE:-}" ] && set -- "$@" --tolerance "$PERF_TOLERANCE"
[ "${PERF_UPDATE:-0}" = 1 ] && set -- "$@" --update
exec python3 scripts/perf_compare.py compare "$@"
