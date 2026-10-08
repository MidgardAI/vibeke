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
#   --allow-downgrade / VIBEKE_ALLOW_DOWNGRADE=1
#                         install a version older than the one installed (`sh -s -- --allow-downgrade`)
#
# Verification: SHA256SUMS must carry a valid minisign signature by one of the release keys below
# (current and next), checked with the `minisign` tool (brew install minisign), and the signature's
# *trusted* comment (signed, unlike the untrusted one) must name the requested version
# (`vibeke v<version>`, as scripts/release-sign.sh writes it), so an older signed release can't be
# replayed under a newer version. The binary must match its SHA256SUMS entry and report the
# requested version. A version older than the installed one is refused unless explicitly allowed.
# Keys: keys/vibeke-2026.pub, keys/vibeke-next.pub in the repo.
set -eu


# Release public keys (keep in sync with crates/vk-remote/src/bootstrap.rs; a test checks this).
KEY_CURRENT="RWQ0X1WPxwluX2gFO4vO586PSTdpSfJqrb+xsQnZ2ctND/VDw7VCWx5z"   # 5F6E09C78F555F34
KEY_NEXT="RWR8LE7QI2pTaSsb4srEFbF1j78fXZzbORy4KGRzHErddJwSJLxwqH3x"      # 69536A23D04E2C7C

VERSION="${VIBEKE_VERSION:-0.2.0}"
BASE_URL="${VIBEKE_RELEASE_URL:-https://github.com/MidgardAI/vibeke/releases/download/v$VERSION}"
API_URL="${VIBEKE_GITHUB_API_URL:-https://api.github.com}"
TOKEN="${VIBEKE_GITHUB_TOKEN:-${GITHUB_TOKEN:-}}"

die() { echo "vibeke install: $*" >&2; exit 1; }

# version_lt A B: A is older than B (numeric dotted parts; a pre-release sorts before its release).
version_lt() {
  awk -v a="$1" -v b="$2" '
    function cmp(x, y,   xa, ya, xp, yp, n, m, i, xn, yn) {
      xp = ""; yp = ""
      if (index(x, "-")) { xp = substr(x, index(x, "-") + 1); x = substr(x, 1, index(x, "-") - 1) }
      if (index(y, "-")) { yp = substr(y, index(y, "-") + 1); y = substr(y, 1, index(y, "-") - 1) }
      sub(/\+.*/, "", x); sub(/\+.*/, "", y)
      n = split(x, xa, "."); m = split(y, ya, "."); if (m > n) n = m
      for (i = 1; i <= n; i++) { xn = xa[i] + 0; yn = ya[i] + 0; if (xn < yn) return -1; if (xn > yn) return 1 }
      if (xp == yp) return 0
      if (xp == "") return 1
      if (yp == "") return -1
      return (xp < yp) ? -1 : 1
    }
    BEGIN { exit (cmp(a, b) < 0) ? 0 : 1 }'
}

refuse_downgrade() { # <version>
  if [ -n "$INSTALLED" ] && version_lt "$1" "$INSTALLED" && [ "$ALLOW_DOWNGRADE" != 1 ]; then
    die "refusing to downgrade from $INSTALLED to $1; nothing installed (pass --allow-downgrade or set VIBEKE_ALLOW_DOWNGRADE=1 to install an older version on purpose)"
  fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else die "need sha256sum or shasum to verify the download"; fi
}

# The token goes only to the configured API base or GitHub's own hosts, matched up to the first
# `/` after the host (so `api.github.com.attacker.example` and `api.github.com@attacker.example`
# never match), never to whatever URL a release listing names.
token_ok() { # <url>
  case "$1" in
    "$API_URL"/*|https://api.github.com/*|https://github.com/*|https://objects.githubusercontent.com/*) return 0 ;;
    *) return 1 ;;
  esac
}

# curl with the token header read from stdin, so it is never visible in argv / ps.
curl_auth() { # <accept> <url> <out>
  token_ok "$2" || die "refusing to send the GitHub token to $2"
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
      curl -fsSL --proto "$(download_proto "$BASE_URL/$1")" --proto-redir '=https' "$BASE_URL/$1" -o "$TMP/$1" || die "download failed: $BASE_URL/$1"
    fi
  elif command -v curl >/dev/null 2>&1; then
    curl -fsSL --proto "$(download_proto "$BASE_URL/$1")" --proto-redir '=https' "$BASE_URL/$1" -o "$TMP/$1" || die "download failed: $BASE_URL/$1 (a private release needs GITHUB_TOKEN or VIBEKE_GITHUB_TOKEN)"
  elif command -v wget >/dev/null 2>&1; then
    case "$(download_proto "$BASE_URL/$1")" in
      '=https') wget -q --https-only "$BASE_URL/$1" -O "$TMP/$1" ;;
      *) wget -q "$BASE_URL/$1" -O "$TMP/$1" ;;
    esac || die "download failed: $BASE_URL/$1 (a private release needs GITHUB_TOKEN or VIBEKE_GITHUB_TOKEN)"
  else
    die "need curl or wget"
  fi
}

# Verify SHA256SUMS.minisig against the embedded release keys (current, then next). On success
# TRUSTED holds the verified trusted comment, as minisign reports it after checking the global
# signature over it (never read from the file: its untrusted comment is not signed at all).
TRUSTED=
verify_signature() {
  [ -f "$TMP/SHA256SUMS.minisig" ] || return 1
  command -v minisign >/dev/null 2>&1 || return 2
  for key in "$KEY_CURRENT" "$KEY_NEXT"; do
    if out=$(minisign -V -P "$key" -m "$TMP/SHA256SUMS" -x "$TMP/SHA256SUMS.minisig" 2>/dev/null); then
      TRUSTED=$(printf '%s\n' "$out" | sed -n 's/^Trusted comment: //p' | head -n 1)
      return 0
    fi
  done
  return 3
}

# Transport restriction for unauthenticated downloads: HTTPS only, except local test servers.
download_proto() { # <url>
  case "$1" in
    http://127.0.0.1[:/]*|http://localhost[:/]*) echo '=http,https' ;;
    *) echo '=https' ;;
  esac
}

# Everything executable lives in main, called on the last line: a truncated download runs nothing.
main() {
  ALLOW_DOWNGRADE="${VIBEKE_ALLOW_DOWNGRADE:-}"
  for arg in "$@"; do
    case "$arg" in
      --allow-downgrade) ALLOW_DOWNGRADE=1 ;;
      *) echo "vibeke install: unknown option $arg" >&2; exit 2 ;;
    esac
  done

  [ -n "${HOME:-}" ] || die "HOME is not set"
  case "$VERSION" in
    ""|*[!0-9A-Za-z.+-]*) die "invalid version '$VERSION'" ;;
  esac
  DATA="$HOME/.local/share/vibeke"
  BIN="$HOME/.local/bin"

  # The installed version (the `current` link), if any.
  INSTALLED=
  if [ -L "$DATA/current" ]; then INSTALLED=$(basename "$(readlink "$DATA/current")"); fi
  refuse_downgrade "$VERSION"
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

  TMP=$(mktemp -d "${TMPDIR:-/tmp}/vibeke-install.XXXXXX")
  trap 'rm -rf "$TMP"' EXIT INT TERM

  echo "vibeke install: $NAME $VERSION"
  fetch "$NAME"
  fetch SHA256SUMS
  ( fetch SHA256SUMS.minisig ) 2>/dev/null || rm -f "$TMP/SHA256SUMS.minisig"  # a missing file is reported below
  EXPECTED="release key 5F6E09C78F555F34 (current) or 69536A23D04E2C7C (next)"
  rc=0; verify_signature || rc=$?
  case $rc in
    0)
      # The signature must be for the requested version: release-sign.sh signs SHA256SUMS with the
      # trusted comment `vibeke v<version>`. An older release's valid signature is refused here.
      case "$TRUSTED" in
        "vibeke v$VERSION"|"vibeke v$VERSION "*) ;;
        *) die "SHA256SUMS is signed for '$TRUSTED', not vibeke v$VERSION (an older or different release served under this version?); nothing installed" ;;
      esac
      echo "vibeke install: SHA256SUMS signature verified (vibeke v$VERSION)" ;;
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
  # The binary must report the requested version (it is never taken from the download instead).
  REPORTED=$("$TMP/$NAME" --version 2>/dev/null | head -n 1 | sed 's/^vibeke //') || true
  [ -n "$REPORTED" ] || die "downloaded binary does not run on this machine"
  [ "$REPORTED" = "$VERSION" ] || die "downloaded binary reports vibeke $REPORTED, not the requested $VERSION; nothing installed"
  refuse_downgrade "$VERSION"

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
  # Replace the link itself, never move into the directory it points at: GNU `mv -T`, BSD `mv -h`.
  mv -fT "$DATA/current.tmp" "$DATA/current" 2>/dev/null \
    || mv -fh "$DATA/current.tmp" "$DATA/current" 2>/dev/null \
    || { rm -f "$DATA/current"; mv "$DATA/current.tmp" "$DATA/current"; }
  [ "$(readlink "$DATA/current")" = "versions/$VERSION" ] || die "could not switch $DATA/current to $VERSION"

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
}

main "$@"
