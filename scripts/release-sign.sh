#!/bin/sh
# Sign a Vibeke release locally (spec 09 §10). Run on the maintainer's Mac, never in CI.
#
#   scripts/release-sign.sh [--key current|next] <dist-dir>
#
# <dist-dir> holds the artifacts built by `scripts/release-build.sh` or downloaded from the draft
# release (vibeke-macos-aarch64, vibeke-linux-x86_64, vibeke-linux-aarch64, ...). The script:
#   1. writes SHA256SUMS over the artifacts and manifest.json (version, per-target sha256 and URL),
#   2. signs both with minisign (SHA256SUMS.minisig, manifest.json.minisig); minisign asks for
#      the key password on the terminal, this script never reads or handles the secret key,
#   3. verifies both signatures against the public key embedded in the binary
#      (crates/vk-remote/src/bootstrap.rs: KEY_CURRENT / KEY_NEXT) before it reports success.
#
#   --key current   sign with ~/.vibeke-release-keys/vibeke-2026.key (default)
#   --key next      sign with ~/.vibeke-release-keys/vibeke-next.key (rotation, see docs/releases.md)
#
# Test hooks (used by scripts/tests/release-sign-test.sh with a throwaway key; not for releases):
#   VIBEKE_SIGN_SECRET_KEY=<file>  VIBEKE_SIGN_PUBLIC_KEY=<base64 line>  override key and pubkey.
#   VIBEKE_SIGN_VERSION=<v>        override the version taken from the directory name / Cargo.toml.
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)
die() { echo "release-sign: $*" >&2; exit 1; }

KEYSEL=current
DIST=
while [ $# -gt 0 ]; do
  case "$1" in
    --key)
      [ $# -ge 2 ] || die "--key needs current or next"
      KEYSEL=$2; shift 2 ;;
    -h|--help) sed -n '2,19p' "$0"; exit 0 ;;
    -*) die "unknown option $1" ;;
    *) [ -z "$DIST" ] || die "one dist directory only"; DIST=$1; shift ;;
  esac
done
[ -n "$DIST" ] || die "usage: release-sign.sh [--key current|next] <dist-dir>"
[ -d "$DIST" ] || die "$DIST is not a directory"
DIST=$(cd "$DIST" && pwd)

BOOT="$ROOT/crates/vk-remote/src/bootstrap.rs"
case "$KEYSEL" in
  current) CONST=KEY_CURRENT; SECRET="$HOME/.vibeke-release-keys/vibeke-2026.key" ;;
  next) CONST=KEY_NEXT; SECRET="$HOME/.vibeke-release-keys/vibeke-next.key" ;;
  *) die "--key must be current or next" ;;
esac
# The embedded public key: what shipped binaries verify against.
PUB=$(sed -n "s/^pub const $CONST: &str = \"\\(.*\\)\";\$/\\1/p" "$BOOT")
[ -n "$PUB" ] || die "cannot read $CONST from $BOOT"
if [ -n "${VIBEKE_SIGN_SECRET_KEY:-}" ]; then
  SECRET=$VIBEKE_SIGN_SECRET_KEY
  PUB=${VIBEKE_SIGN_PUBLIC_KEY:?VIBEKE_SIGN_PUBLIC_KEY is required with VIBEKE_SIGN_SECRET_KEY}
  echo "release-sign: TEST MODE: using $SECRET and a non-release public key" >&2
fi

command -v minisign >/dev/null 2>&1 || die "minisign not found (brew install minisign)"
[ -f "$SECRET" ] || die "secret key $SECRET not found"

# Version: VIBEKE_SIGN_VERSION, else a semver-looking directory name, else Cargo.toml.
cargo_version() { sed -n '/^\[workspace\.package\]/,/^\[/{s/^version *= *"\(.*\)"/\1/p;}' "$ROOT/Cargo.toml" | head -n 1; }
VERSION=${VIBEKE_SIGN_VERSION:-}
if [ -z "$VERSION" ]; then
  base=$(basename "$DIST")
  case "$base" in
    [0-9]*.[0-9]*.[0-9]*) VERSION=$base ;;
    *) VERSION=$(cargo_version) ;;
  esac
fi
[ -n "$VERSION" ] || die "cannot determine the version"
BASE_URL="${VIBEKE_RELEASE_URL:-https://github.com/MidgardAI/vibeke/releases/download/v$VERSION}"

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

# Artifacts: vibeke-<os>-<arch> files, not checksums or signatures.
ARTIFACTS=
for f in "$DIST"/vibeke-*; do
  [ -f "$f" ] || continue
  case "$f" in *.sha256|*.minisig) continue ;; esac
  ARTIFACTS="$ARTIFACTS $(basename "$f")"
done
[ -n "$ARTIFACTS" ] || die "no vibeke-* artifacts in $DIST"

cd "$DIST"
rm -f SHA256SUMS SHA256SUMS.minisig manifest.json manifest.json.minisig
for a in $ARTIFACTS; do echo "$(sha256 "$a")  $a"; done > SHA256SUMS

# manifest.json: what `bootstrap = "remote-download"` trusts. target = file name without "vibeke-".
{
  printf '{"version":"%s","artifacts":[' "$VERSION"
  first=1
  for a in $ARTIFACTS; do
    [ "$first" = 1 ] || printf ','
    first=0
    printf '{"target":"%s","sha256":"%s","url":"%s/%s"}' "${a#vibeke-}" "$(sha256 "$a")" "$BASE_URL" "$a"
  done
  printf ']}\n'
} > manifest.json

echo "release-sign: version $VERSION, key: $KEYSEL, artifacts:$ARTIFACTS"
echo "release-sign: signing SHA256SUMS (minisign asks for the key password)"
minisign -S -s "$SECRET" -m SHA256SUMS -t "vibeke v$VERSION" || die "signing SHA256SUMS failed"
echo "release-sign: signing manifest.json (password again)"
# The trusted comment names the version: the client refuses a manifest whose signature is for
# another version (no replay under a new label).
minisign -S -s "$SECRET" -m manifest.json -t "vibeke v$VERSION version:$VERSION" || die "signing manifest.json failed"

# Verify with the public key the binary embeds, not with the key we just used.
for f in SHA256SUMS manifest.json; do
  minisign -V -P "$PUB" -m "$f" -x "$f.minisig" >/dev/null || die "$f.minisig does not verify against the embedded $KEYSEL key"
done
case $(cat manifest.json.minisig) in
  *"version:$VERSION"*) ;;
  *) die "manifest.json.minisig trusted comment lacks version:$VERSION" ;;
esac
echo "release-sign: verified SHA256SUMS and manifest.json against the embedded $KEYSEL key"
echo
echo "Upload these files to the draft release, then publish it:"
for f in SHA256SUMS SHA256SUMS.minisig manifest.json manifest.json.minisig; do echo "  $DIST/$f"; done
