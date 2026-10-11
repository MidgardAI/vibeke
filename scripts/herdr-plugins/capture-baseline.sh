#!/bin/sh
# Captures the pending Herdr 0.9.3 baseline entries (spec 18 §11) from the reference binary.
#
#   capture-baseline.sh <herdr-binary> <expected-sha256> <out-dir>
#
# Verifies the binary's sha256 BEFORE running it, then writes, in the layout that
# tests/compat/herdr/0.9.3/baseline.toml names:
#
#   <out>/api-schema.json          `herdr api schema --json`
#   <out>/cli-help/herdr.txt       `herdr --help`
#   <out>/cli-help/<a>_<b>.txt     every `herdr <a> [<b> …] --help`, found by walking the
#                                  "Commands:" sections of each help page
#   <out>/cli-help/index.txt       the captured commands and their exit codes
#   <out>/reference-binary.sha256
#
# Herdr runs with a fresh HOME, XDG and TMPDIR and an empty environment. A person reviews the
# output and commits it; nothing here edits baseline.toml.
set -eu

[ "$#" -eq 3 ] || { echo "usage: capture-baseline.sh <herdr-binary> <sha256> <out-dir>" >&2; exit 2; }
bin=$1
expected=$2
out=$3
case $bin in /*) ;; *) echo "the binary path must be absolute" >&2; exit 2 ;; esac

if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "$bin" | awk '{print $1}')
else
  actual=$(shasum -a 256 "$bin" | awk '{print $1}')
fi
if [ "$actual" != "$expected" ]; then
  echo "capture-baseline.sh: sha256 mismatch for $bin: $actual, expected $expected" >&2
  exit 1
fi

[ ! -e "$out" ] || { echo "capture-baseline.sh: $out already exists" >&2; exit 2; }
mkdir -p "$out/cli-help"
root=$(mktemp -d "${RUNNER_TEMP:-/tmp}/herdr-baseline.XXXXXX")
mkdir -p "$root/home" "$root/config" "$root/data" "$root/state" "$root/cache" "$root/tmp" "$root/run"
chmod 700 "$root/run"

herdr() {
  env -i HOME="$root/home" PATH=/usr/bin:/bin TERM=dumb LANG=C LC_ALL=C TZ=UTC NO_COLOR=1 \
    XDG_CONFIG_HOME="$root/config" XDG_DATA_HOME="$root/data" XDG_STATE_HOME="$root/state" \
    XDG_CACHE_HOME="$root/cache" XDG_RUNTIME_DIR="$root/run" TMPDIR="$root/tmp" \
    "$bin" "$@" </dev/null
}

version=$(herdr --version)
case $version in
  *0.9.3*) ;;
  *) echo "capture-baseline.sh: unexpected version: $version" >&2; exit 1 ;;
esac
printf '%s  herdr\n' "$actual" >"$out/reference-binary.sha256"

herdr api schema --json >"$out/api-schema.json"

index=$out/cli-help/index.txt
: >"$index"
capture() {
  # $1: file stem; the rest: the command words.
  stem=$1
  shift
  status=0
  herdr "$@" --help >"$out/cli-help/$stem.txt" 2>&1 || status=$?
  printf 'herdr %s --help\texit %s\n' "$*" "$status" >>"$index"
}

# The subcommand names in a clap "Commands:" section: two-space indent, then a name.
subcommands() {
  awk '
    /^[A-Za-z].*:$/ { in_cmds = ($0 == "Commands:") ; next }
    in_cmds && /^  [a-z][a-z0-9-]*( |$)/ { print $1 }
    in_cmds && /^$/ { in_cmds = 0 }
  ' "$1"
}

capture herdr
# The top-level nouns at the pinned commit (src/cli/spec.rs). The root help is Herdr's own text,
# so the nouns are listed here; deeper levels come from clap's help pages.
for noun in completion update status config channel machine server api workspace worktree tab \
  notification agent pane terminal session integration plugin; do
  capture "$noun" "$noun"
  for verb in $(subcommands "$out/cli-help/$noun.txt"); do
    capture "${noun}_$verb" "$noun" "$verb"
    for sub in $(subcommands "$out/cli-help/${noun}_$verb.txt"); do
      capture "${noun}_${verb}_$sub" "$noun" "$verb" "$sub"
    done
  done
done

echo "captured $(wc -l <"$index" | tr -d ' ') help pages and the API schema into $out"
