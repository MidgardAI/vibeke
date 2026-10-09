#!/bin/sh
# Build the release artifacts for <version> reproducibly into dist/<version>/. Builds only; it
# neither signs nor publishes (see scripts/release-sign.sh and docs/releases.md).
#
#   scripts/release-build.sh <version> [--repro-check] [--only macos|linux]
#
# Targets (a target whose toolchain is missing is skipped with a notice):
#   vibeke-macos-aarch64   aarch64-apple-darwin, natively on an Apple-silicon Mac
#   vibeke-linux-x86_64    x86_64-unknown-linux-musl   via cargo-zigbuild + zig
#   vibeke-linux-aarch64   aarch64-unknown-linux-musl  via cargo-zigbuild + zig
#
# The build environment is the one scripts/repro-check.sh uses (SOURCE_DATE_EPOCH from the commit,
# --remap-path-prefix, --locked, stripped symbols). `--repro-check` additionally runs
# scripts/repro-check.sh for the Linux targets first, so a non-reproducible toolchain fails early.
# Run through `mise exec -- sh scripts/release-build.sh <version>` so rust, zig and cargo-zigbuild
# are on PATH. `--only macos|linux` builds just that family (the release workflow builds each on its own runner). Output: dist/<version>/{vibeke-*, vibeke-*.sha256, SHA256SUMS}.
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)
die() { echo "release-build: $*" >&2; exit 1; }

VERSION=${1:-}
[ -n "$VERSION" ] || die "usage: release-build.sh <version> [--repro-check]"
shift
REPRO=0
ONLY=all
while [ $# -gt 0 ]; do
  case "$1" in
    --repro-check) REPRO=1; shift ;;
    --only)
      [ $# -ge 2 ] || die "--only needs macos or linux"
      ONLY=$2; shift 2 ;;
    *) die "unknown option $1" ;;
  esac
done
case "$ONLY" in all|macos|linux) ;; *) die "--only must be macos or linux" ;; esac

CARGO_VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -n 1)
[ "$VERSION" = "$CARGO_VERSION" ] || die "version $VERSION does not match the workspace version $CARGO_VERSION in Cargo.toml"

# Same reproducibility environment as scripts/repro-check.sh.
SOURCE_DATE_EPOCH="${SOURCE_DATE_EPOCH:-$(git log -1 --format=%ct)}"
export SOURCE_DATE_EPOCH
CARGO_HOME_DIR="${CARGO_HOME:-$HOME/.cargo}"
RUSTUP_HOME_DIR="${RUSTUP_HOME:-$HOME/.rustup}"
export RUSTFLAGS="--remap-path-prefix=$ROOT=/build --remap-path-prefix=$CARGO_HOME_DIR=/cargo --remap-path-prefix=$RUSTUP_HOME_DIR=/rustup -C strip=symbols"
export CARGO_INCREMENTAL=0
# Build libghostty-vt from source: a cached archive would skip what releases must reproduce.
export VK_TERM_CACHE_DIR=0
export TZ=UTC LC_ALL=C

HAVE_ZIG=0
if command -v zig >/dev/null 2>&1 && command -v cargo-zigbuild >/dev/null 2>&1; then HAVE_ZIG=1; fi

if [ "$REPRO" = 1 ]; then
  [ "$HAVE_ZIG" = 1 ] || die "--repro-check needs zig and cargo-zigbuild"
  echo "==> repro-check (Linux musl)"
  REPRO_TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl" sh scripts/repro-check.sh
fi

OUT="$ROOT/dist/$VERSION"
rm -rf "$OUT"
mkdir -p "$OUT"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

BUILT=
# usage: build <artifact> <rust target> <cargo|zigbuild>
build() {
  name=$1; target=$2; builder=$3
  echo "==> $name ($target)"
  if [ "$builder" = zigbuild ]; then
    cargo zigbuild --release --locked -p vibeke --target "$target"
  else
    cargo build --release --locked -p vibeke --target "$target"
  fi
  cp "target/$target/release/vibeke" "$OUT/$name"
  chmod 755 "$OUT/$name"
  echo "$(sha256 "$OUT/$name")  $name" > "$OUT/$name.sha256"
  BUILT="$BUILT $name"
}

if [ "$ONLY" = linux ]; then
  :
elif [ "$(uname -s)-$(uname -m)" = "Darwin-arm64" ]; then
  build vibeke-macos-aarch64 aarch64-apple-darwin cargo
else
  echo "release-build: skipping vibeke-macos-aarch64 (needs an Apple-silicon Mac)"
fi
if [ "$ONLY" = macos ]; then
  :
elif [ "$HAVE_ZIG" = 1 ]; then
  build vibeke-linux-x86_64 x86_64-unknown-linux-musl zigbuild
  build vibeke-linux-aarch64 aarch64-unknown-linux-musl zigbuild
else
  echo "release-build: skipping the Linux targets (zig or cargo-zigbuild not found; use mise)"
fi

[ -n "$BUILT" ] || die "nothing was built"
(cd "$OUT" && cat vibeke-*.sha256 > SHA256SUMS)
echo
echo "built:$BUILT"
echo "SOURCE_DATE_EPOCH=$SOURCE_DATE_EPOCH"
ls -l "$OUT"
echo "Not signed and not published. Next: scripts/release-sign.sh dist/$VERSION"
