#!/bin/sh
# Tests for scripts/release-sign.sh and the signature / token handling of scripts/install.sh.
# Uses THROWAWAY keys generated in a temp dir (`minisign -G -W`, no password). The real release
# keys are never read: HOME points at the temp dir, so ~/.vibeke-release-keys does not exist.
# Skipped (exit 0) when minisign is not installed.
#
#   sh scripts/tests/release-sign-test.sh
set -eu

command -v minisign >/dev/null 2>&1 || { echo "release-sign-test: minisign not installed, skipping"; exit 0; }

HERE=$(cd "$(dirname "$0")" && pwd)
SIGN="$HERE/../release-sign.sh"
INSTALL="$HERE/../install.sh"
WORK=$(mktemp -d "${TMPDIR:-/tmp}/release-sign-test.XXXXXX")
SERVER_PID=
cleanup() {
  if [ -n "$SERVER_PID" ]; then kill "$SERVER_PID" 2>/dev/null || true; wait "$SERVER_PID" 2>/dev/null || true; fi
  rm -rf "$WORK"
}
trap cleanup EXIT

fail() { echo "FAIL: $*" >&2; exit 1; }
ok() { echo "ok: $*"; }

export HOME="$WORK/home"
mkdir -p "$HOME"

genkey() { # <name>  ->  $WORK/<name>.key / .pub, prints the base64 public key line
  minisign -G -W -f -p "$WORK/$1.pub" -s "$WORK/$1.key" >/dev/null 2>&1
  sed -n '2p' "$WORK/$1.pub"
}
# The installer's embedded keys, the Rust keys and keys/*.pub must all agree.
BOOT="$HERE/../../crates/vk-remote/src/bootstrap.rs"
for pair in "KEY_CURRENT:vibeke-2026" "KEY_NEXT:vibeke-next"; do
  c=${pair%%:*}; f=${pair##*:}
  rs=$(sed -n "s/^pub const $c: &str = \"\(.*\)\";\$/\1/p" "$BOOT")
  sh_key=$(sed -n "s/^$c=\"\([^\"]*\)\".*/\1/p" "$INSTALL")
  pub=$(sed -n '2p' "$HERE/../../keys/$f.pub")
  [ -n "$rs" ] && [ "$rs" = "$sh_key" ] && [ "$rs" = "$pub" ] || fail "$c differs between bootstrap.rs, install.sh and keys/$f.pub"
done
ok "embedded keys agree (bootstrap.rs, install.sh, keys/*.pub)"

PUB_A=$(genkey a)
PUB_B=$(genkey b)

# --- release-sign.sh ------------------------------------------------------------------------
DIST="$WORK/dist/0.9.0"
mkdir -p "$DIST"
echo linux-x86 >"$DIST/vibeke-linux-x86_64"
echo linux-arm >"$DIST/vibeke-linux-aarch64"
echo mac >"$DIST/vibeke-macos-aarch64"
echo stale >"$DIST/vibeke-macos-aarch64.sha256"

# Without the key file (HOME is empty) it refuses before doing anything.
if sh "$SIGN" "$DIST" >"$WORK/out" 2>&1; then fail "signed without a key file"; fi
grep -q "secret key .* not found" "$WORK/out" || fail "missing key file message: $(cat "$WORK/out")"
if sh "$SIGN" --key next "$DIST" >"$WORK/out" 2>&1; then fail "signed with --key next without a key"; fi
grep -q "vibeke-next.key" "$WORK/out" || grep -q "not found" "$WORK/out" || fail "--key next: $(cat "$WORK/out")"
[ ! -e "$DIST/SHA256SUMS.minisig" ] || fail "a signature appeared without a key"
ok "refuses without a key file (current and next)"

if sh "$SIGN" --key bogus "$DIST" >/dev/null 2>&1; then fail "accepted --key bogus"; fi
ok "rejects an unknown --key"

VIBEKE_SIGN_SECRET_KEY="$WORK/a.key" VIBEKE_SIGN_PUBLIC_KEY="$PUB_A" sh "$SIGN" "$DIST" >"$WORK/out" 2>&1 \
  || fail "signing with the throwaway key: $(cat "$WORK/out")"
for f in SHA256SUMS SHA256SUMS.minisig manifest.json manifest.json.minisig; do
  [ -s "$DIST/$f" ] || fail "$f missing"
done
grep -q "^$(shasum -a 256 "$DIST/vibeke-linux-x86_64" | cut -d' ' -f1)  vibeke-linux-x86_64\$" "$DIST/SHA256SUMS" || fail "SHA256SUMS content"
grep -q "vibeke-macos-aarch64.sha256" "$DIST/SHA256SUMS" && fail "sidecar listed in SHA256SUMS"
[ "$(wc -l <"$DIST/SHA256SUMS" | tr -d ' ')" = 3 ] || fail "SHA256SUMS should list 3 artifacts"
grep -q '"version":"0.9.0"' "$DIST/manifest.json" || fail "manifest version"
grep -q '"target":"linux-x86_64"' "$DIST/manifest.json" || fail "manifest target"
grep -q 'releases/download/v0.9.0/vibeke-linux-x86_64' "$DIST/manifest.json" || fail "manifest url"
grep -q "trusted comment: vibeke v0.9.0\$" "$DIST/SHA256SUMS.minisig" || fail "SHA256SUMS trusted comment"
grep -q "trusted comment: vibeke v0.9.0 version:0.9.0\$" "$DIST/manifest.json.minisig" || fail "manifest trusted comment"
minisign -V -P "$PUB_A" -m "$DIST/SHA256SUMS" -x "$DIST/SHA256SUMS.minisig" >/dev/null || fail "SHA256SUMS does not verify"
if minisign -V -P "$PUB_B" -m "$DIST/SHA256SUMS" -x "$DIST/SHA256SUMS.minisig" >/dev/null 2>&1; then fail "verifies with the wrong key"; fi
echo tampered >>"$DIST/SHA256SUMS"
if minisign -V -P "$PUB_A" -m "$DIST/SHA256SUMS" -x "$DIST/SHA256SUMS.minisig" >/dev/null 2>&1; then fail "tampered sums verify"; fi
ok "signs SHA256SUMS and manifest.json, verifies, detects tampering"

# --- install.sh (keys swapped for the throwaway ones; the script itself is otherwise unchanged) ---
mkinstaller() { # <current pub> <next pub> -> $WORK/install.sh
  sed -e "s|^KEY_CURRENT=\"[^\"]*\"|KEY_CURRENT=\"$1\"|" -e "s|^KEY_NEXT=\"[^\"]*\"|KEY_NEXT=\"$2\"|" "$INSTALL" >"$WORK/install.sh"
  grep -q "KEY_CURRENT=\"$1\"" "$WORK/install.sh" || fail "could not patch KEY_CURRENT"
}
REL="$WORK/rel"
mkrelease() { # <signing key name>
  rm -rf "$REL"; mkdir -p "$REL"
  case "$(uname -s)-$(uname -m)" in
    Darwin-arm64) NAME=vibeke-macos-aarch64 ;;
    Linux-x86_64) NAME=vibeke-linux-x86_64 ;;
    Linux-aarch64|Linux-arm64) NAME=vibeke-linux-aarch64 ;;
    *) echo "release-sign-test: installer tests need a supported platform, skipping them"; exit 0 ;;
  esac
  printf '#!/bin/sh\necho "vibeke 0.9.0"\n' >"$REL/$NAME"
  chmod 755 "$REL/$NAME"
  echo "$(shasum -a 256 "$REL/$NAME" | cut -d' ' -f1)  $NAME" >"$REL/SHA256SUMS"
  [ -z "${1:-}" ] || minisign -S -s "$WORK/$1.key" -m "$REL/SHA256SUMS" -t "vibeke v0.9.0" >/dev/null
}
install_run() { # runs the installer into a fresh HOME; extra env via the caller
  rm -rf "$WORK/ihome"; mkdir -p "$WORK/ihome"
  HOME="$WORK/ihome" VIBEKE_INSTALL_FROM="$REL" sh "$WORK/install.sh" >"$WORK/iout" 2>&1
}

mkinstaller "$PUB_A" "$PUB_B"
mkrelease a
install_run || fail "installer with a signed release: $(cat "$WORK/iout")"
grep -q "signature verified" "$WORK/iout" || fail "no verification message"
ok "installer accepts a release signed by the current key"

mkrelease b
install_run || fail "installer with the next key: $(cat "$WORK/iout")"
ok "installer accepts a release signed by the next key (rotation)"

mkinstaller "$PUB_A" "$PUB_A"
if install_run; then fail "installer accepted a signature by an unknown key"; fi
grep -q "5F6E09C78F555F34" "$WORK/iout" && grep -q "69536A23D04E2C7C" "$WORK/iout" || fail "error does not name the key ids: $(cat "$WORK/iout")"
[ ! -e "$WORK/ihome/.local/bin/vibeke" ] || fail "installed despite a bad signature"
ok "installer refuses an unknown signing key and names the expected key ids"

mkinstaller "$PUB_A" "$PUB_B"
mkrelease ""
if install_run; then fail "installer accepted an unsigned release"; fi
grep -q "SHA256SUMS.minisig is missing" "$WORK/iout" || fail "unsigned message: $(cat "$WORK/iout")"
[ ! -e "$WORK/ihome/.local/bin/vibeke" ] || fail "installed an unsigned release"
VIBEKE_ALLOW_UNSIGNED=1 install_run || fail "opt-in did not allow: $(cat "$WORK/iout")"
grep -q "WARNING" "$WORK/iout" || fail "no warning for the unsigned opt-in"
ok "unsigned release refused; VIBEKE_ALLOW_UNSIGNED=1 accepts it with a warning"

# The opt-in does not waive the checksum.
echo "$(printf '0%.0s' $(seq 64))  $NAME" >"$REL/SHA256SUMS"
if VIBEKE_ALLOW_UNSIGNED=1 install_run; then fail "opt-in waived the checksum"; fi
grep -q "checksum mismatch" "$WORK/iout" || fail "checksum message"
ok "checksum still required with the opt-in"

mkrelease a
echo "evil" >>"$REL/$NAME"
if install_run; then fail "installed a binary that does not match the signed sums"; fi
ok "binary not matching the signed SHA256SUMS is refused"

# --- token: GitHub API asset download, header sent, never printed ---------------------------
if command -v python3 >/dev/null 2>&1 && command -v curl >/dev/null 2>&1; then
  mkrelease a
  cp "$REL/SHA256SUMS.minisig" "$WORK/srv.minisig"
  cat >"$WORK/server.py" <<'PY'
import http.server, os, sys, json, socketserver
rel = os.environ["REL"]; log = os.environ["LOG"]; name = os.environ["NAME"]
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_GET(self):
        with open(log, "a") as f:
            f.write("%s\nAuthorization: %s\nAccept: %s\n--\n" % (self.path, self.headers.get("Authorization"), self.headers.get("Accept")))
        base = "http://127.0.0.1:%d" % self.server.server_port
        if self.path == "/repos/o/r/releases/tags/v0.9.0":
            files = [name, "SHA256SUMS", "SHA256SUMS.minisig"]
            body = json.dumps({"assets": [{"url": "%s/repos/o/r/releases/assets/%d" % (base, i + 1), "id": i + 1, "name": n} for i, n in enumerate(files)]}, indent=2).encode()
        elif self.path.startswith("/repos/o/r/releases/assets/"):
            n = [name, "SHA256SUMS", "SHA256SUMS.minisig"][int(self.path.rsplit("/", 1)[1]) - 1]
            body = open(os.path.join(rel, n), "rb").read()
        else:
            self.send_response(404); self.end_headers(); return
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)
class S(http.server.HTTPServer):
    def server_bind(self):  # HTTPServer.server_bind does a slow reverse DNS lookup
        socketserver.TCPServer.server_bind(self)
        self.server_name = "127.0.0.1"; self.server_port = self.socket.getsockname()[1]
s = S(("127.0.0.1", 0), H)
print(s.server_port, flush=True)
s.serve_forever()
PY
  : >"$WORK/req.log"
  REL="$REL" LOG="$WORK/req.log" NAME="$NAME" python3 "$WORK/server.py" >"$WORK/port" &
  SERVER_PID=$!
  i=0; while [ ! -s "$WORK/port" ] && [ $i -lt 50 ]; do sleep 0.1; i=$((i + 1)); done
  PORT=$(cat "$WORK/port")
  mkinstaller "$PUB_A" "$PUB_B"
  rm -rf "$WORK/ihome"; mkdir -p "$WORK/ihome"
  if ! HOME="$WORK/ihome" VIBEKE_RELEASE_URL="https://github.com/o/r/releases/download/v0.9.0" \
    VIBEKE_GITHUB_API_URL="http://127.0.0.1:$PORT" VIBEKE_GITHUB_TOKEN="ghp_TESTTOKEN123" \
    sh "$WORK/install.sh" >"$WORK/iout" 2>&1; then fail "token install: $(cat "$WORK/iout")"; fi
  grep -q "Authorization: Bearer ghp_TESTTOKEN123" "$WORK/req.log" || fail "token header not sent: $(cat "$WORK/req.log")"
  grep -q "Accept: application/octet-stream" "$WORK/req.log" || fail "octet-stream Accept not sent"
  grep -q "ghp_TESTTOKEN123" "$WORK/iout" && fail "token printed by the installer"
  ok "private-repo install: asset API with Authorization and octet-stream Accept; token not printed"
else
  echo "release-sign-test: python3 or curl missing, skipping the token test"
fi

echo "release-sign-test: all passed"
