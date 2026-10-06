#!/bin/sh
# Exit-status tests for scripts/repro-check.sh. Stub cargo, cargo-zigbuild, zig and (per case)
# sha256sum are put first on PATH, so no real build runs. POSIX sh; run from anywhere:
#
#   sh scripts/tests/repro-check-test.sh
set -eu

HERE=$(cd "$(dirname "$0")" && pwd)
SCRIPT="$HERE/../repro-check.sh"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/repro-check-test.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

# --- stubs -------------------------------------------------------------------------------------
STUBS="$WORK/stubs"
mkdir -p "$STUBS"

# cargo zigbuild ... --target T: behaviour selected by STUB_CARGO=ok|fail|fail2|noartifact|differ.
cat >"$STUBS/cargo" <<'STUB'
#!/bin/sh
target=
while [ $# -gt 0 ]; do
  [ "$1" = --target ] && { target=$2; shift; }
  shift
done
run=${CARGO_TARGET_DIR##*-}
echo "stub cargo: run $run target $target"
case ${STUB_CARGO:-ok} in
  fail) echo "error: stub build failure" >&2; exit 101 ;;
  fail2) [ "$run" = 2 ] && { echo "error: stub build 2 failure" >&2; exit 101; } ;;
  noartifact) exit 0 ;;
esac
mkdir -p "$CARGO_TARGET_DIR/$target/release"
if [ "${STUB_CARGO:-ok}" = differ ]; then
  echo "binary from run $run" >"$CARGO_TARGET_DIR/$target/release/vibeke"
else
  echo "identical binary" >"$CARGO_TARGET_DIR/$target/release/vibeke"
fi
STUB
printf '#!/bin/sh\nexit 0\n' >"$STUBS/cargo-zigbuild"
printf '#!/bin/sh\nexit 0\n' >"$STUBS/zig"

# sha256sum variants, each in its own dir so a case can opt in.
mkdir -p "$WORK/sha-fail" "$WORK/sha-empty" "$WORK/sha-garbage" "$WORK/sha-short"
printf '#!/bin/sh\necho "sha256sum: stub failure" >&2\nexit 1\n' >"$WORK/sha-fail/sha256sum"
printf '#!/bin/sh\nexit 0\n' >"$WORK/sha-empty/sha256sum"
# shellcheck disable=SC2016 # $1 is meant for the stub, not this shell
printf '#!/bin/sh\necho "not-a-hash  $1"\n' >"$WORK/sha-garbage/sha256sum"
# shellcheck disable=SC2016
printf '#!/bin/sh\necho "abc123  $1"\n' >"$WORK/sha-short/sha256sum"
chmod +x "$STUBS"/* "$WORK"/sha-*/sha256sum

# --- runner ------------------------------------------------------------------------------------
pass=0
failed=0

report() { # report NAME WANT GOT OUTPUT_FILE
  if [ "$3" -eq "$2" ]; then
    echo "ok   - $1 (exit $3)"
    pass=$((pass + 1))
  else
    echo "FAIL - $1: expected exit $2, got $3"
    sed 's/^/       | /' "$4"
    failed=$((failed + 1))
  fi
}

# check NAME EXPECTED_STATUS STUB_CARGO [SHA_DIR]
check() {
  path="$STUBS:$PATH"
  [ -z "${4:-}" ] || path="$WORK/$4:$path"
  out="$WORK/out.log"
  set +e
  env PATH="$path" STUB_CARGO="$3" SOURCE_DATE_EPOCH=0 TMPDIR="$WORK" \
    REPRO_TARGETS=x86_64-unknown-linux-musl sh "$SCRIPT" >"$out" 2>&1
  got=$?
  set -e
  report "$1" "$2" "$got" "$out"
}

check "identical artifacts are reproducible"          0 ok
check "differing artifacts are reported"              1 differ
check "both builds failing is an error"               3 fail
check "second build failing is an error"              3 fail2
check "builds succeed but artifact absent"            3 noartifact
check "checksum command fails"                        3 ok sha-fail
check "checksum command prints nothing"               3 ok sha-empty
check "checksum command prints a non-hex digest"      3 ok sha-garbage
check "checksum command prints a short digest"        3 ok sha-short

# Prerequisite check: no cargo-zigbuild/zig on PATH -> exit 2.
mkdir -p "$WORK/nozig"
cp "$STUBS/cargo" "$WORK/nozig/cargo"
out="$WORK/out.log"
set +e
env PATH="$WORK/nozig:/usr/bin:/bin" SOURCE_DATE_EPOCH=0 TMPDIR="$WORK" sh "$SCRIPT" >"$out" 2>&1
got=$?
set -e
report "missing cargo-zigbuild is a prerequisite error" 2 "$got" "$out"

echo "repro-check tests: $pass passed, $failed failed"
[ "$failed" -eq 0 ]
