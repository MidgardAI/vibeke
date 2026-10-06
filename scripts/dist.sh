#!/bin/sh
# Build release binaries and publish them locally (dist/<version>/ and the release cache that
# `vibeke ssh` pushes from). Run through `mise run dist` so rust, zig and cargo-zigbuild are on PATH.
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)

VERSION=$(sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' Cargo.toml | head -n 1)
[ -n "$VERSION" ] || { echo "dist: cannot read workspace version from Cargo.toml" >&2; exit 1; }

OUT="$ROOT/dist/$VERSION"
CACHE="${VIBEKE_RELEASES_DIR:-$HOME/.cache/vibeke/releases}/$VERSION"
rm -rf "$OUT"
mkdir -p "$OUT" "$CACHE"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# usage: build <artifact name> <rust target> <cargo|zigbuild>
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
}

build vibeke-macos-aarch64 aarch64-apple-darwin cargo
build vibeke-linux-x86_64  x86_64-unknown-linux-musl zigbuild
build vibeke-linux-aarch64 aarch64-unknown-linux-musl zigbuild

(cd "$OUT" && cat vibeke-*.sha256 > SHA256SUMS)

cp "$OUT"/vibeke-* "$CACHE"/
cp "$OUT/SHA256SUMS" "$CACHE"/

echo
echo "version $VERSION"
ls -l "$OUT"
echo "copied to $CACHE"
