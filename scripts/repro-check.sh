#!/bin/sh
# Reproducible-build check (spec 09 §10, spec 11 M6): build the Linux musl artifacts twice into
# separate target dirs with SOURCE_DATE_EPOCH, --remap-path-prefix and --locked, then compare
# sha256. Run through `mise run repro-check` so rust, zig and cargo-zigbuild are on PATH.
#
#   REPRO_TARGETS   space-separated targets (default: x86_64-unknown-linux-musl)
#   REPRO_KEEP=1    keep the scratch build directories
#
# Exit 0: identical. Exit 1: differing digests (both are printed). Exit 2: prerequisites missing.
# Exit 3: a build failed, an artifact is missing, or a checksum could not be computed.
# Tests: scripts/tests/repro-check-test.sh (stubs cargo/zig/sha256sum on PATH).
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

command -v cargo-zigbuild >/dev/null 2>&1 || { echo "repro-check: cargo-zigbuild not found (mise install)" >&2; exit 2; }
command -v zig >/dev/null 2>&1 || { echo "repro-check: zig not found (mise install)" >&2; exit 2; }

TARGETS="${REPRO_TARGETS:-x86_64-unknown-linux-musl}"
# Fixed timestamp: the HEAD commit time unless the caller pins one (releases use the tag commit).
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"
export SOURCE_DATE_EPOCH
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
RUSTUP_HOME_DIR="${RUSTUP_HOME:-$HOME/.rustup}"

# Paths that would otherwise be embedded (panic locations, debug info, build-script output).
RUSTFLAGS_COMMON="--remap-path-prefix=$ROOT=/build --remap-path-prefix=$CARGO_HOME_DIR=/cargo --remap-path-prefix=$RUSTUP_HOME_DIR=/rustup -C strip=symbols"
export CARGO_INCREMENTAL=0
# Build libghostty-vt from source: a cached archive would skip what releases must reproduce.
export VK_TERM_CACHE_DIR=0
export TZ=UTC LC_ALL=C

SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/vibeke-repro.XXXXXX")
[ "${REPRO_KEEP:-}" = 1 ] || trap 'rm -rf "$SCRATCH"' EXIT

fail() { echo "repro-check: $*" >&2; exit 3; }

# Prints the sha256 of $1, or exits 3. No pipelines: each command's status is checked directly,
# and the result must be exactly 64 lowercase hex digits.
sha256() {
  [ -f "$1" ] || fail "artifact not found: $1"
  if command -v sha256sum >/dev/null 2>&1; then
    out=$(sha256sum "$1") || fail "sha256sum failed for $1"
  else
    out=$(shasum -a 256 "$1") || fail "shasum failed for $1"
  fi
  hash=${out%%[[:space:]]*}
  case $hash in
    *[!0-9a-f]*|'') fail "malformed checksum for $1: '$out'" ;;
  esac
  [ ${#hash} -eq 64 ] || fail "malformed checksum for $1: '$out'"
  printf '%s\n' "$hash"
}

echo "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
echo "RUSTFLAGS=$RUSTFLAGS_COMMON (+ target dir remap)"
status=0
for target in $TARGETS; do
  for run in 1 2; do
    echo "==> $target build $run"
    # Different target dirs, same source path: the remap hides the target dir too.
    # Output goes to a log (not a pipe) so cargo's exit status is not masked.
    log="$SCRATCH/build-$target-$run.log"
    if RUSTFLAGS="$RUSTFLAGS_COMMON --remap-path-prefix=$SCRATCH/target-$run=/target" \
      CARGO_TARGET_DIR="$SCRATCH/target-$run" \
      cargo zigbuild --release --locked -p vibeke --target "$target" >"$log" 2>&1; then
      tail -3 "$log"
    else
      rc=$?
      tail -30 "$log" >&2
      fail "$target build $run failed (cargo exit $rc)"
    fi
  done
  # Command substitution in an assignment propagates sha256's exit status, so set -e stops here.
  a=$(sha256 "$SCRATCH/target-1/$target/release/vibeke")
  b=$(sha256 "$SCRATCH/target-2/$target/release/vibeke")
  echo "$a  $target (build 1)"
  echo "$b  $target (build 2)"
  if [ "$a" = "$b" ]; then
    echo "REPRODUCIBLE: $target"
  else
    echo "DIFFERENT: $target" >&2
    status=1
  fi
done
exit $status
