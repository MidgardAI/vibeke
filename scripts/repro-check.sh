#!/bin/sh
# Reproducible-build check (spec 09 §10, spec 11 M6): build the Linux musl artifacts twice into
# separate target dirs with SOURCE_DATE_EPOCH, --remap-path-prefix and --locked, then compare
# sha256. Run through `mise run repro-check` so rust, zig and cargo-zigbuild are on PATH.
#
#   REPRO_TARGETS   space-separated targets (default: x86_64-unknown-linux-musl)
#   REPRO_KEEP=1    keep the scratch build directories
#
# Exit 0: identical. Exit 1: differing digests (both are printed). Exit 2: prerequisites missing.
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
export TZ=UTC LC_ALL=C

SCRATCH=$(mktemp -d "${TMPDIR:-/tmp}/vibeke-repro.XXXXXX")
[ "${REPRO_KEEP:-}" = 1 ] || trap 'rm -rf "$SCRATCH"' EXIT

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

echo "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
echo "RUSTFLAGS=$RUSTFLAGS_COMMON (+ target dir remap)"
status=0
for target in $TARGETS; do
  for run in 1 2; do
    echo "==> $target build $run"
    # Different target dirs, same source path: the remap hides the target dir too.
    RUSTFLAGS="$RUSTFLAGS_COMMON --remap-path-prefix=$SCRATCH/target-$run=/target" \
      CARGO_TARGET_DIR="$SCRATCH/target-$run" \
      cargo zigbuild --release --locked -p vibeke --target "$target" 2>&1 | tail -3
  done
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
