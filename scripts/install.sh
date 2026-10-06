#!/bin/sh
# Vibeke installer: curl -fsSL <url>/install.sh | sh
#
# Installs into ~/.local/share/vibeke/versions/<v>/vibeke, points ~/.local/share/vibeke/current at
# it and links ~/.local/bin/vibeke. Never uses sudo, never writes outside $HOME.
#
#   VIBEKE_VERSION        version to install (default: the version this script was released with)
#   VIBEKE_RELEASE_URL    base URL holding vibeke-<os>-<arch> and SHA256SUMS
#                         (default: https://github.com/MidgardAI/vibeke/releases/download/v<version>)
#   VIBEKE_INSTALL_FROM   local directory with the same files, for offline installs
set -eu

VERSION="${VIBEKE_VERSION:-0.1.0}"
BASE_URL="${VIBEKE_RELEASE_URL:-https://github.com/MidgardAI/vibeke/releases/download/v$VERSION}"

die() { echo "vibeke install: $*" >&2; exit 1; }

[ -n "${HOME:-}" ] || die "HOME is not set"
[ "$(id -u)" -ne 0 ] || echo "vibeke install: running as root; installing for root's HOME ($HOME)" >&2

case "$(uname -s)" in
  Darwin) OS=macos ;;
  Linux) OS=linux ;;
  *) die "unsupported OS $(uname -s) (macOS and Linux only)" ;;
esac
case "$(uname -m)" in
  arm64|aarch64) ARCH=aarch64 ;;
  x86_64|amd64) ARCH=x86_64 ;;
  *) die "unsupported architecture $(uname -m)" ;;
esac
NAME="vibeke-$OS-$ARCH"
case "$NAME" in
  vibeke-macos-aarch64|vibeke-linux-aarch64|vibeke-linux-x86_64) ;;
  *) die "no release build for $OS/$ARCH" ;;
esac

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum to verify the download"; fi
}

TMP=$(mktemp -d "${TMPDIR:-/tmp}/vibeke-install.XXXXXX")
trap 'rm -rf "$TMP"' EXIT INT TERM

fetch() { # <name>
  if [ -n "${VIBEKE_INSTALL_FROM:-}" ]; then
    [ -f "$VIBEKE_INSTALL_FROM/$1" ] || die "$VIBEKE_INSTALL_FROM/$1 not found"
    cp "$VIBEKE_INSTALL_FROM/$1" "$TMP/$1"
  elif command -v curl >/dev/null 2>&1; then
    curl -fsSL "$BASE_URL/$1" -o "$TMP/$1" || die "download failed: $BASE_URL/$1"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$BASE_URL/$1" -O "$TMP/$1" || die "download failed: $BASE_URL/$1"
  else
    die "need curl or wget"
  fi
}

echo "vibeke install: $NAME $VERSION"
fetch "$NAME"
fetch SHA256SUMS

WANT=$(awk -v n="$NAME" '$2 == n || $2 == "*" n { print $1; exit }' "$TMP/SHA256SUMS")
[ -n "$WANT" ] || die "$NAME is not listed in SHA256SUMS"
GOT=$(sha256 "$TMP/$NAME")
[ "$WANT" = "$GOT" ] || die "checksum mismatch for $NAME (expected $WANT, got $GOT); nothing installed"
echo "vibeke install: sha256 verified"

chmod 755 "$TMP/$NAME"
# The binary reports its own version; use that for the directory name.
REPORTED=$("$TMP/$NAME" --version 2>/dev/null | head -n 1 | sed 's/^vibeke //') || true
[ -n "$REPORTED" ] || die "downloaded binary does not run on this machine"
VERSION="$REPORTED"

DATA="$HOME/.local/share/vibeke"
BIN="$HOME/.local/bin"
DIR="$DATA/versions/$VERSION"
mkdir -p "$DIR" "$BIN"
chmod 700 "$DATA"
cp "$TMP/$NAME" "$DIR/vibeke.tmp"
chmod 755 "$DIR/vibeke.tmp"
mv -f "$DIR/vibeke.tmp" "$DIR/vibeke"

# Remember the previous version for `vibeke update --rollback`.
if [ -L "$DATA/current" ]; then
  PREV=$(basename "$(readlink "$DATA/current")")
  [ "$PREV" = "$VERSION" ] || echo "$PREV" > "$DATA/previous"
fi
ln -sfn "versions/$VERSION" "$DATA/current.tmp"
mv -f "$DATA/current.tmp" "$DATA/current" 2>/dev/null || { rm -f "$DATA/current"; mv "$DATA/current.tmp" "$DATA/current"; }

if [ -e "$BIN/vibeke" ] && [ ! -L "$BIN/vibeke" ]; then
  mv "$BIN/vibeke" "$BIN/vibeke.old"
  echo "vibeke install: existing $BIN/vibeke kept as vibeke.old"
fi
ln -sfn "$DATA/current/vibeke" "$BIN/vibeke.tmp"
mv -f "$BIN/vibeke.tmp" "$BIN/vibeke"

echo "vibeke install: installed $("$BIN/vibeke" --version) at $BIN/vibeke"
case ":$PATH:" in
  *":$BIN:"*) ;;
  *)
    echo
    echo "$BIN is not on your PATH. Add this to your shell profile:"
    echo "  export PATH=\"\$HOME/.local/bin:\$PATH\""
    ;;
esac
echo
echo "Next: vibeke doctor"
