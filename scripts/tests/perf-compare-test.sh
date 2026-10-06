#!/bin/sh
# Self-test for scripts/perf_compare.py (the relative perf gate): a slower machine is not a
# regression, a real regression fails, noisy samples and a missing baseline never fail.
# Fast, no cargo.
set -eu
here=$(cd "$(dirname "$0")" && pwd)
tool="$here/../perf_compare.py"
tmp=$(mktemp -d "${TMPDIR:-/tmp}/perf-compare-test.XXXXXX")
trap 'rm -rf "$tmp"' EXIT
plat=$(python3 -c 'import platform;print(f"{platform.system().lower()}-{platform.machine().lower()}")')

# mk DIR SPAWN_MS CPU_MS COLD_MS VT... : N samples with the same numbers (cold_start in $4,
# one vt value per remaining arg, one sample each).
mk() {
  dir=$1; spawn=$2; cpu=$3; cold=$4; shift 4
  i=0
  for vt in "$@"; do
    i=$((i + 1))
    mkdir -p "$dir/s$i"
    printf '{"cpu_ms": %s, "spawn_ms": %s}\n' "$cpu" "$spawn" >"$dir/s$i/cal.json"
    printf 'cold_start\t%s\n' "$cold" >"$dir/s$i/timing.tsv"
    printf '%s\n' "$vt" >"$dir/s$i/vt.txt"
  done
}
# Baseline: spawn 100 ms, cpu 50 ms, cold start 50 ms (ratio 0.5), VT 1000 MB/s (ratio 50000).
cat >"$tmp/base.json" <<EOF
{"tolerance": 1.0, "platforms": {"$plat": {"cold_start": 0.5, "vt_throughput_mb_s": 50000}}}
EOF
run() { python3 "$tool" compare "$1" --baseline "$tmp/base.json" >"$tmp/out.txt" 2>&1; }
expect() { # name want
  set +e; run "$tmp/$1"; rc=$?; set -e
  [ "$rc" = "$2" ] || { echo "FAIL $1: exit $rc, wanted $2"; cat "$tmp/out.txt"; exit 1; }
}

mk "$tmp/same" 100 50 50 1000 1000 1000
expect same 0
# A machine twice as slow in every respect: raw numbers double, ratios do not move.
mk "$tmp/slowbox" 200 100 100 500 500 500
expect slowbox 0
# A 3x slowdown of cold start on an unchanged machine: regression.
mk "$tmp/regress" 100 50 150 1000 1000 1000
expect regress 1
# VT throughput collapsing to a third: regression (higher is better).
mk "$tmp/vtdrop" 100 50 50 330 330 330
expect vtdrop 1
# Within tolerance (+50% with +100% allowed).
mk "$tmp/mild" 100 50 75 1000 1000 1000
expect mild 0
# Samples that disagree by more than 3x: too noisy to fail on.
mkdir -p "$tmp/noisy"
for n in 1 2 3 4 5; do
  mkdir -p "$tmp/noisy/s$n"
  printf '{"cpu_ms": 50, "spawn_ms": 100}\n' >"$tmp/noisy/s$n/cal.json"
done
for pair in 1:20 2:25 3:200 4:250 5:300; do
  n=${pair%%:*}; v=${pair##*:}
  printf 'cold_start\t%s\n' "$v" >"$tmp/noisy/s$n/timing.tsv"
done
expect noisy 0
grep -q NOISY "$tmp/out.txt" || { echo "FAIL noisy: no NOISY verdict"; cat "$tmp/out.txt"; exit 1; }
# Too few samples: nothing to gate.
mk "$tmp/few" 100 50 500 1000
expect few 0
# No baseline for this platform: report only, candidate written.
echo '{"platforms": {"plan9-mips": {}}}' >"$tmp/otherbase.json"
set +e
python3 "$tool" compare "$tmp/regress" --baseline "$tmp/otherbase.json" --candidate "$tmp/cand.json" >"$tmp/out.txt" 2>&1
rc=$?
set -e
[ "$rc" = 0 ] && [ -s "$tmp/cand.json" ] || { echo "FAIL nobaseline"; cat "$tmp/out.txt"; exit 1; }
echo "perf-compare-test: ok"
