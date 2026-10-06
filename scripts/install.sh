#!/bin/sh
# Vibeke installer: curl -fsSL <url>/install.sh | sh
#
# Installs into ~/.local/share/vibeke/versions/<v>/vibeke, points ~/.local/share/vibeke/current at
# it and links ~/.local/bin/vibeke. Never uses sudo, never writes outside $HOME.
#
#   VIBEKE_VERSION        version to install (default: the version this script was released with)
#   VIBEKE_RELEASE_URL    base URL holding vibeke-<os>-<arch>, SHA256SUMS and SHA256SUMS.minisig
#                         (default: https://github.com/MidgardAI/vibeke/releases/download/v<version>)
#   VIBEKE_INSTALL_FROM   local directory with the same files, for offline installs
#   GITHUB_TOKEN / VIBEKE_GITHUB_TOKEN
#                         only for a private release repo: downloads then go through the GitHub
#                         API asset endpoint. Public releases need no token. The token is never
#                         printed and never placed on a command line (curl reads it from stdin).
#   VIBEKE_ALLOW_UNSIGNED=1
#                         development only: accept a download whose SHA256SUMS signature cannot
#                         be verified (the checksum must still match)
#
# Verification: SHA256SUMS must carry a valid minisign signature by one of the release keys below
# (current and next), checked with the `minisign` tool (brew install minisign), then the binary
# must match its SHA256SUMS entry. Keys: keys/vibeke-2026.pub, keys/vibeke-next.pub in the repo.
set -eu

# Release public keys (keep in sync with crates/vk-remote/src/bootstrap.rs; a test checks this).
KEY_CURRENT="RWQ0X1WPxwluX2gFO4vO586PSTdpSfJqrb+xsQnZ2ctND/VDw7VCWx5z"   # 5F6E09C78F555F34
KEY_NEXT="RWR8LE7QI2pTaSsb4srEFbF1j78fXZzbORy4KGRzHErddJwSJLxwqH3x"      # 69536A23D04E2C7C

VERSION="${VIBEKE_VERSION:-0.1.0}"
BASE_URL="${VIBEKE_RELEASE_URL:-https://github.com/MidgardAI/vibeke/releases/download/v$VERSION}"
API_URL="${VIBEKE_GITHUB_API_URL:-https://api.github.com}"
TOKEN="${VIBEKE_GITHUB_TOKEN:-${GITHUB_TOKEN:-}}"

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

# curl with the token header read from stdin, so it is never visible in argv / ps.
curl_auth() { # <accept> <url> <out>
  case "$2" in
    http://127.0.0.1[:/]*|http://localhost[:/]*) proto='=http,https' ;;  # local test servers only
    *) proto='=https' ;;
  esac
  printf 'header = "Authorization: Bearer %s"\nheader = "Accept: %s"\n' "$TOKEN" "$1" |
    curl -fsSL --proto "$proto" --proto-redir '=https' -K - "$2" -o "$3"
}

# Asset API URL for <name> of a github.com release URL (private repos need the API endpoint).
github_asset_url() { # <name>
  rest=${BASE_URL#https://github.com/}
  [ "$rest" != "$BASE_URL" ] || return 1
  owner=${rest%%/*}; rest=${rest#*/}
  repo=${rest%%/*}; rest=${rest#*/}
  case "$rest" in releases/download/*) tag=${rest#releases/download/}; tag=${tag%%/*} ;; *) return 1 ;; esac
  curl_auth "application/vnd.github+json" "$API_URL/repos/$owner/$repo/releases/tags/$tag" "$TMP/release.json" || return 1
  # Each asset's API "url" precedes its "name" in the (pretty-printed) release JSON.
  awk -v n="$1" '
    /"url": "[^"]*\/releases\/assets\/[0-9]+"/ { u=$0; sub(/.*"url": "/, "", u); sub(/".*/, "", u) }
    $0 ~ "\"name\": \"" n "\"" { print u; exit }' "$TMP/release.json"
}

fetch() { # <name>
  if [ -n "${VIBEKE_INSTALL_FROM:-}" ]; then
    [ -f "$VIBEKE_INSTALL_FROM/$1" ] || die "$VIBEKE_INSTALL_FROM/$1 not found"
    cp "$VIBEKE_INSTALL_FROM/$1" "$TMP/$1"
  elif [ -n "$TOKEN" ]; then
    command -v curl >/dev/null 2>&1 || die "a GitHub token needs curl"
    if url=$(github_asset_url "$1") && [ -n "$url" ]; then
      curl_auth "application/octet-stream" "$url" "$TMP/$1" || die "download failed: $1 (via the GitHub API; check the token)"
    else
      # Not a github.com release URL (a mirror): the token is not sent there.
      curl -fsSL "$BASE_URL/$1" -o "$TMP/$1" || die "download failed: $BASE_URL/$1"
    fi
  elif command -v curl >/dev/null 2>&1; then
    curl -fsSL "$BASE_URL/$1" -o "$TMP/$1" || die "download failed: $BASE_URL/$1 (a private release needs GITHUB_TOKEN or VIBEKE_GITHUB_TOKEN)"
  elif command -v wget >/dev/null 2>&1; then
    wget -q "$BASE_URL/$1" -O "$TMP/$1" || die "download failed: $BASE_URL/$1 (a private release needs GITHUB_TOKEN or VIBEKE_GITHUB_TOKEN)"
  else
    die "need curl or wget"
  fi
}

# Verify SHA256SUMS.minisig against the embedded release keys (current, then next).
verify_signature() {
  [ -f "$TMP/SHA256SUMS.minisig" ] || return 1
  command -v minisign >/dev/null 2>&1 || return 2
  minisign -V -P "$KEY_CURRENT" -m "$TMP/SHA256SUMS" -x "$TMP/SHA256SUMS.minisig" >/dev/null 2>&1 && return 0
  minisign -V -P "$KEY_NEXT" -m "$TMP/SHA256SUMS" -x "$TMP/SHA256SUMS.minisig" >/dev/null 2>&1 && return 0
  return 3
}

echo "vibeke install: $NAME $VERSION"
fetch "$NAME"
fetch SHA256SUMS
( fetch SHA256SUMS.minisig ) 2>/dev/null || rm -f "$TMP/SHA256SUMS.minisig"  # a missing file is reported below
EXPECTED="release key 5F6E09C78F555F34 (current) or 69536A23D04E2C7C (next)"
rc=0; verify_signature || rc=$?
case $rc in
  0) echo "vibeke install: SHA256SUMS signature verified" ;;
  *)
    case $rc in
      1) why="SHA256SUMS.minisig is missing from the release" ;;
      2) why="minisign is not installed (brew install minisign, or your package manager)" ;;
      *) why="the signature does not verify" ;;
    esac
    if [ "${VIBEKE_ALLOW_UNSIGNED:-}" = 1 ]; then
      echo "vibeke install: WARNING: VIBEKE_ALLOW_UNSIGNED=1: continuing without a verified signature ($why); only the checksum protects this install" >&2
    else
      die "cannot verify the release: $why; expected a signature by $EXPECTED. Nothing installed. (VIBEKE_ALLOW_UNSIGNED=1 accepts an unsigned release for development)"
    fi
    ;;
esac

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
