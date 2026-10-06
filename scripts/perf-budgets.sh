#!/bin/sh
# Run the existing latency / throughput / bandwidth measurements and compare them with the
# spec 10 §1 budgets. REPORT-ONLY: always exits 0 unless PERF_STRICT=1 (then 1 on any FAIL).
#
#   mise run perf-budgets
#   PERF_MACHINE=devbox mise run perf-budgets     # also the remote bandwidth budgets (10 §1.5)
#   PERF_BROWSER=1 mise run perf-budgets          # also the vk-browser frame-path bench
#
# Budget gates are only meaningful on the reference machines (10 §2.1); elsewhere treat the
# numbers as indicative. Raw tool output is kept in $PERF_OUT (default target/perf-budgets).
set -u
OUT="${PERF_OUT:-target/perf-budgets}"
mkdir -p "$OUT"
REPORT="$OUT/report.txt"
: > "$REPORT"
FAILS=0

row() { # name measured budget verdict
  printf '%-44s %-18s %-22s %s\n' "$1" "$2" "$3" "$4" | tee -a "$REPORT"
}
# verdict_le VALUE LIMIT -> PASS/FAIL (value <= limit); verdict_ge VALUE LIMIT (value >= limit)
verdict_le() { awk -v v="$1" -v l="$2" 'BEGIN{print (v<=l)?"PASS":"FAIL"}'; }
verdict_ge() { awk -v v="$1" -v l="$2" 'BEGIN{print (v>=l)?"PASS":"FAIL"}'; }
note() { [ "$1" = FAIL ] && FAILS=$((FAILS+1)); echo "$1"; }

row "metric" "measured" "budget (spec 10)" "verdict"
row "------" "--------" "----------------" "-------"

# 1.2 VT parse throughput (vk-term C5 test, release build).
if cargo test --release -p vk-term --test recovery throughput -- --ignored --nocapture \
    > "$OUT/vt-throughput.txt" 2>&1; then
  mbs=$(sed -n 's/.*throughput: \([0-9]*\) MB\/s.*/\1/p' "$OUT/vt-throughput.txt" | head -1)
  if [ -n "$mbs" ]; then
    v=$(note "$(verdict_ge "$mbs" 300)")
    row "VT parse throughput per pane (1.2)" "$mbs MB/s" ">= 300 MB/s" "$v"
  else
    row "VT parse throughput per pane (1.2)" "unparsed" ">= 300 MB/s" "SKIP"
  fi
else
  row "VT parse throughput per pane (1.2)" "test failed" ">= 300 MB/s" "SKIP"
fi

# 1.1 Added keystroke latency: needs a reachable local server; skipped when there is none.
if cargo build --release -p vibeke > "$OUT/build.txt" 2>&1; then
  BIN="${CARGO_TARGET_DIR:-target}/release/vibeke"
  # Isolated session so the probe never touches (or leaves behind) a real server.
  LAT_DIR=$(mktemp -d /tmp/vkperf.XXXXXX)
  if VIBEKE_RUNTIME_DIR="$LAT_DIR/run" VIBEKE_STATE_DIR="$LAT_DIR/state" VIBEKE_CONFIG="$LAT_DIR/c.toml" \
      "$BIN" debug latency --n "${PERF_LATENCY_N:-300}" > "$OUT/latency.txt" 2>&1; then
    line=$(grep '^added:' "$OUT/latency.txt" | head -1)
    p50=$(echo "$line" | sed -n 's/.*p50 *\([0-9.]*\) ms.*/\1/p')
    p99=$(echo "$line" | sed -n 's/.*p99 *\([0-9.]*\) ms.*/\1/p')
    if [ -n "$p50" ] && [ -n "$p99" ]; then
      row "Keystroke -> screen added p50 (1.1)" "$p50 ms" "<= 1 ms" "$(note "$(verdict_le "$p50" 1)")"
      row "Keystroke -> screen added p99 (1.1)" "$p99 ms" "<= 3 ms" "$(note "$(verdict_le "$p99" 3)")"
    else
      row "Keystroke -> screen added (1.1)" "unparsed" "p50<=1, p99<=3 ms" "SKIP"
    fi
  else
    row "Keystroke -> screen added (1.1)" "no server" "p50<=1, p99<=3 ms" "SKIP (start one: vibeke server)"
  fi
else
  row "Keystroke -> screen added (1.1)" "build failed" "p50<=1, p99<=3 ms" "SKIP"
fi

VIBEKE_RUNTIME_DIR="${LAT_DIR:-/nonexistent}/run" VIBEKE_STATE_DIR="${LAT_DIR:-/nonexistent}/state" \
  VIBEKE_CONFIG="${LAT_DIR:-/nonexistent}/c.toml" "${CARGO_TARGET_DIR:-target}/release/vibeke" server stop \
  >/dev/null 2>&1
[ -n "${LAT_DIR:-}" ] && rm -rf "$LAT_DIR"

# 1.3 Idle CPU / RSS / wakeups: isolated server with 30 idle panes + a headless attached TUI
# (`vibeke debug idle`). Verdicts carry "(loaded)" when the host load exceeds half the cores;
# do not read budget compliance from a loaded run. PERF_IDLE=0 skips it (about 3 minutes).
if [ "${PERF_IDLE:-1}" = 1 ] && [ -x "${CARGO_TARGET_DIR:-target}/release/vibeke" ]; then
  BIN="${CARGO_TARGET_DIR:-target}/release/vibeke"
  if "$BIN" debug idle --seconds "${PERF_IDLE_SECONDS:-15}" --repeat "${PERF_IDLE_REPEAT:-3}" \
      > "$OUT/idle.txt" 2>&1; then
    while IFS= read -r l; do
      case "$l" in *"(loaded)"*) ;; *" FAIL"*) FAILS=$((FAILS+1));; esac
    done < "$OUT/idle.txt"
    row "Idle CPU/RSS/wakeups (1.3)" "see idle.txt" "10 §1.3 rows" "REVIEW"
    sed 's/^/    /' "$OUT/idle.txt" | tee -a "$REPORT"
  else
    row "Idle CPU/RSS/wakeups (1.3)" "failed" "10 §1.3 rows" "SKIP"
  fi
else
  row "Idle CPU/RSS/wakeups (1.3)" "-" "release build needed (or PERF_IDLE=0)" "SKIP"
fi

# 6 VT conformance suite (in-repo corpus; also runs in `mise run test`). Baseline counts only.
VK_CONFORMANCE_REPORT=1 cargo test -p vk-term --test conformance -- --nocapture \
  > "$OUT/conformance.txt" 2>&1
tot=$(sed -n 's/^TOTAL \(.*\)/\1/p' "$OUT/conformance.txt" | head -1)
if [ -n "$tot" ]; then
  row "VT conformance corpus (6)" "$tot" "0 unexpected failures" "REVIEW"
else
  row "VT conformance corpus (6)" "failed or unparsed" "0 unexpected failures" "FAIL"
  FAILS=$((FAILS+1))
fi

# 1.5 Remote bandwidth: needs an SSH-reachable machine with vibeke installed.
if [ -n "${PERF_MACHINE:-}" ]; then
  BIN="${CARGO_TARGET_DIR:-target}/release/vibeke"
  if "$BIN" debug bandwidth --machine "$PERF_MACHINE" --seconds "${PERF_BW_SECONDS:-20}" \
      > "$OUT/bandwidth.txt" 2>&1; then
    row "Remote bandwidth (1.5)" "see bandwidth.txt" "idle 0 B/s; spinner <=2 KiB/s unfocused, <=8 KiB/s focused" "REVIEW"
    sed 's/^/    /' "$OUT/bandwidth.txt" | tee -a "$REPORT"
  else
    row "Remote bandwidth (1.5)" "failed" "see budgets" "SKIP"
  fi
else
  row "Remote bandwidth (1.5)" "-" "set PERF_MACHINE=<label>" "SKIP"
fi

# 1.1/1.6 browser frame path (Goal 03 stage-0 bench); needs a Chromium build.
if [ "${PERF_BROWSER:-0}" = 1 ]; then
  if cargo run --release -p vk-browser --example bench -- --quick > "$OUT/browser-bench.txt" 2>&1; then
    row "Browser frame-path bench" "see browser-bench.txt" "10 §1.6" "REVIEW"
  else
    row "Browser frame-path bench" "failed" "10 §1.6" "SKIP"
  fi
fi

echo
echo "report: $REPORT   (report-only; PERF_STRICT=1 turns FAIL into exit 1)"
if [ "${PERF_STRICT:-0}" = 1 ] && [ "$FAILS" -gt 0 ]; then
  exit 1
fi
exit 0
