#!/bin/sh
# Runs one command as the unprivileged boundary user of a plugin job (spec 18 §5).
#
#   run-boundary.sh --user <name> --root <dir> --timeout <seconds> [--path <dirs>]
#                   [--env NAME=VALUE]... [--env-file <file>]... [--workdir <dir>] -- <command> [args]...
#
# The caller is the trusted runner user with passwordless sudo. The script:
#
# 1. creates fresh HOME, XDG, TMPDIR and runtime directories under <root> (which must not exist),
#    owned by the boundary user with mode 700;
# 2. builds the environment from nothing (`env -i`): a fixed base, the given PATH, and the
#    --env and --env-file entries. The base sets CI=true and
#    VIBEKE_HERDR_PLUGINS_I_AM_IN_A_BOUNDARY=1, which the runner's safety gate requires: this
#    script is the boundary. Caller entries must use a name from a fixed allowlist (the harness
#    settings and the dependency proxy names that the plugin runner passes through, see
#    proxy-env.sh). Names that look like credentials, GitHub or runner variables, SSH agents or
#    cloud keys are refused as well, so a caller mistake cannot leak them;
# 3. runs the command as the boundary user with stdin closed and a global timeout;
# 4. kills every process the boundary user still has, whatever happened.
#
# Process cleanup is best effort only. The real cleanup is that the runner is destroyed after
# the job (18 §5.4). Exit status: the command's, 124 on timeout, 125 on a usage or setup error.
set -eu

die() {
  echo "run-boundary.sh: $*" >&2
  exit 125
}

user=""
root=""
timeout=""
path_dirs=""
workdir=""
tmp=$(mktemp -d "${RUNNER_TEMP:-/tmp}/run-boundary.XXXXXX") || die "mktemp failed"
envs="$tmp/env"
: >"$envs"
# shellcheck disable=SC2329 # invoked by the trap
cleanup() {
  rm -f "$tmp/env" "$tmp/base" "$tmp/timed-out"
  rmdir "$tmp" 2>/dev/null || true
}
trap cleanup EXIT

valid_entry() {
  case $1 in
    *=*) ;;
    *) die "not NAME=VALUE: $1" ;;
  esac
  name=${1%%=*}
  case $name in
    '' | [0-9]* | *[!A-Za-z0-9_]*) die "invalid variable name: $name" ;;
  esac
  upper=$(printf '%s' "$name" | tr '[:lower:]' '[:upper:]')
  case $upper in
    GITHUB_* | ACTIONS_* | RUNNER_* | INPUT_* | SSH_AUTH_SOCK | SSH_AGENT_PID | GPG_AGENT_INFO \
      | AWS_* | AZURE_* | ARM_* | GOOGLE_* | GCP_* | GCLOUD_* | CLOUDSDK_* | DIGITALOCEAN_* \
      | *TOKEN* | *SECRET* | *PASSWORD* | *PASSWD* | *CREDENTIAL* | *_KEY | *_KEY_ID | *APIKEY* \
      | DOCKER_* | KUBECONFIG | VAULT_*)
      die "refusing to pass $name into the boundary"
      ;;
  esac
  case $name in
    VIBEKE_HERDR_PLUGINS | VIBEKE_PLUGIN_MIRROR | VIBEKE_HERDR_PLUGIN_RESULTS | VIBEKE_HERDR_PLUGINS_ROOT \
      | VIBEKE_HERDR_PLUGINS_TOOLCHAIN_PATH | VIBEKE_HERDR_BIN | VIBEKE_HERDR_SHA256 \
      | RUSTUP_HOME | RUSTUP_TOOLCHAIN | CARGO_TERM_COLOR | NEXTEST_HIDE_PROGRESS_BAR \
      | GIT_CONFIG_COUNT | GIT_CONFIG_KEY_0 | GIT_CONFIG_VALUE_0 \
      | HTTPS_PROXY | HTTP_PROXY | ALL_PROXY | NO_PROXY | https_proxy | http_proxy | all_proxy | no_proxy \
      | SSL_CERT_FILE | NODE_EXTRA_CA_CERTS | CARGO_HTTP_CAINFO | GIT_SSL_CAINFO | REQUESTS_CA_BUNDLE \
      | CURL_CA_BUNDLE | GOPROXY | GOFLAGS | GOSUMDB) ;;
    *) die "$name is not on the boundary environment allowlist" ;;
  esac
}

add_env() {
  valid_entry "$1"
  printf '%s\n' "$1" >>"$envs"
}

while [ "$#" -gt 0 ]; do
  case $1 in
    --user) user=$2; shift 2 ;;
    --root) root=$2; shift 2 ;;
    --timeout) timeout=$2; shift 2 ;;
    --path) path_dirs=$2; shift 2 ;;
    --workdir) workdir=$2; shift 2 ;;
    --env) add_env "$2"; shift 2 ;;
    --env-file)
      [ -f "$2" ] || die "no env file $2"
      while IFS= read -r line || [ -n "$line" ]; do
        case $line in '' | '#'*) continue ;; esac
        add_env "$line"
      done <"$2"
      shift 2
      ;;
    --) shift; break ;;
    *) die "unknown option $1" ;;
  esac
done
[ -n "$user" ] && [ -n "$root" ] && [ -n "$timeout" ] || die "--user, --root and --timeout are required"
[ "$#" -gt 0 ] || die "no command"
case $timeout in '' | *[!0-9]*) die "--timeout must be whole seconds" ;; esac
case $root in /*) ;; *) die "--root must be absolute" ;; esac
case $user in '' | *[!a-z0-9_-]*) die "invalid user name" ;; esac
id "$user" >/dev/null 2>&1 || die "no user $user"
uid=$(id -u "$user")
[ "$uid" -ne 0 ] || die "the boundary user must not be root"
[ "$uid" -ne "$(id -u)" ] || die "the boundary user must differ from the caller"
[ ! -e "$root" ] || die "$root already exists; every run needs fresh directories"

group=$(id -gn "$user")
home=$root/home
sudo -n mkdir -p "$home" "$root/config" "$root/data" "$root/state" "$root/cache" "$root/tmp" "$root/run"
sudo -n chown -R "$user:$group" "$root"
sudo -n chmod 700 "$root" "$home" "$root/config" "$root/data" "$root/state" "$root/cache" "$root/tmp" "$root/run"
[ -n "$workdir" ] || workdir=$home

case $(uname -s) in
  Darwin) lang=en_US.UTF-8; system_path=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin ;;
  *) lang=C.UTF-8; system_path=/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin ;;
esac
path=$system_path
[ -z "$path_dirs" ] || path=$path_dirs:$system_path

# The base environment. Caller entries come later, and `env` lets the last entry win.
base="$tmp/base"
cat >"$base" <<EOF
HOME=$home
USER=$user
LOGNAME=$user
SHELL=/bin/sh
PATH=$path
LANG=$lang
LC_ALL=$lang
TZ=UTC
TERM=xterm-256color
TMPDIR=$root/tmp
XDG_CONFIG_HOME=$root/config
XDG_DATA_HOME=$root/data
XDG_STATE_HOME=$root/state
XDG_CACHE_HOME=$root/cache
XDG_RUNTIME_DIR=$root/run
CARGO_HOME=$home/.cargo
GOPATH=$home/go
GOTOOLCHAIN=local
CI=true
VIBEKE_HERDR_PLUGINS_I_AM_IN_A_BOUNDARY=1
EOF

command_text=$(printf '%s ' "$@" | cut -c 1-400)

# Prepend `env -i <entries>` to the command: append the entries, then rotate the command's
# arguments to the end. POSIX sh has no arrays.
n=$#
while IFS= read -r line; do
  set -- "$@" "$line"
done <"$base"
while IFS= read -r line; do
  set -- "$@" "$line"
done <"$envs"
# shellcheck disable=SC2016 # expanded by the inner shell
set -- "$@" /bin/sh -c 'umask 077; cd -- "$0" || exit 125; exec "$@"' "$workdir"
while [ "$n" -gt 0 ]; do
  a=$1
  shift
  set -- "$@" "$a"
  n=$((n - 1))
done

kill_user() {
  # Best effort, a few rounds: a process can fork while the previous round runs.
  for _ in 1 2 3; do
    sudo -n pkill -KILL -u "$user" 2>/dev/null || true
    sudo -n pkill -KILL -U "$user" 2>/dev/null || true
  done
}

flag="$tmp/timed-out"
echo "run-boundary.sh: running as $user with a ${timeout}s limit: $command_text" >&2
sudo -n -u "$user" -- /usr/bin/env -i "$@" </dev/null &
pid=$!
(
  sleep "$timeout"
  : >"$flag"
  kill_user
) </dev/null >/dev/null 2>&1 &
watchdog=$!

status=0
wait "$pid" || status=$?
pkill -P "$watchdog" 2>/dev/null || true
kill "$watchdog" 2>/dev/null || true
kill_user
if [ -e "$flag" ]; then
  echo "run-boundary.sh: timed out after ${timeout}s; killed every process of $user" >&2
  status=124
fi
left=$(pgrep -u "$user" 2>/dev/null | wc -l | tr -d ' ')
[ "$left" = 0 ] || echo "run-boundary.sh: $left processes of $user survived cleanup" >&2
exit "$status"
