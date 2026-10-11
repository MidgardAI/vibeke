#!/bin/sh
# Prepares the execution boundary of a plugin job (spec 18 §5). Run by the trusted runner user,
# which has passwordless sudo, before any plugin code runs.
#
#   setup-boundary.sh user <name>              create an unprivileged user and group and close
#                                              the runner user's home to other users
#   setup-boundary.sh seal <dir>...            make trusted files root-owned and read-only
#   setup-boundary.sh firewall <name> <mode>   mode `loopback`: the user may reach only loopback
#                                              (verified dependency mode, 18 §7.2);
#                                              mode `no-link-local`: block link-local addresses
#                                              such as cloud metadata services
#   setup-boundary.sh trust-ca <pem>           trust the dependency proxy's CA system-wide where
#                                              tools ignore SSL_CERT_FILE (macOS Go and Swift)
#
# Each step is fail-closed except `trust-ca`, which only widens what works.
set -eu

die() {
  echo "setup-boundary.sh: $*" >&2
  exit 1
}

valid_name() {
  case $1 in '' | *[!a-z0-9_-]*) die "invalid user name $1" ;; esac
}

create_user() {
  name=$1
  valid_name "$name"
  if id "$name" >/dev/null 2>&1; then
    die "user $name already exists"
  fi
  case $(uname -s) in
    Darwin)
      # The first id from 700 that is free as both a user and a group id.
      ids=$( (dscl . -list /Users UniqueID; dscl . -list /Groups PrimaryGroupID) | awk '{print $2}')
      id_=700
      while printf '%s\n' "$ids" | grep -qx "$id_"; do id_=$((id_ + 1)); done
      sudo -n dscl . -create "/Groups/$name"
      sudo -n dscl . -create "/Groups/$name" PrimaryGroupID "$id_"
      sudo -n dscl . -create "/Users/$name"
      sudo -n dscl . -create "/Users/$name" UniqueID "$id_"
      sudo -n dscl . -create "/Users/$name" PrimaryGroupID "$id_"
      sudo -n dscl . -create "/Users/$name" UserShell /usr/bin/false
      sudo -n dscl . -create "/Users/$name" NFSHomeDirectory /var/empty
      sudo -n dscl . -create "/Users/$name" RealName "Vibeke plugin boundary"
      sudo -n dscl . -create "/Users/$name" IsHidden 1
      # Directory services can take a moment to answer for the new user.
      tries=0
      until id "$name" >/dev/null 2>&1; do
        tries=$((tries + 1))
        [ "$tries" -lt 50 ] || die "user $name did not appear"
        sleep 0.2
      done
      ;;
    Linux)
      sudo -n useradd --system --user-group --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin "$name"
      ;;
    *) die "unsupported OS $(uname -s)" ;;
  esac
  [ "$(id -u "$name")" -ne 0 ] || die "user $name has uid 0"
  if id -Gn "$name" | tr ' ' '\n' | grep -Eqx 'sudo|admin|wheel|docker|adm|root'; then
    die "user $name is in a privileged group"
  fi
  # The runner user's home holds the checkout, tool caches and runner files. The boundary user
  # works only from root-owned copies elsewhere.
  sudo -n chmod 700 "$HOME"
  echo "created boundary user $name ($(id "$name"))"
}

seal() {
  for dir in "$@"; do
    case $dir in /*) ;; *) die "seal needs absolute paths" ;; esac
    [ -e "$dir" ] || die "no such path $dir"
    sudo -n chown -R 0:0 "$dir"
    sudo -n chmod -R a+rX,go-w "$dir"
  done
}

firewall() {
  name=$1
  mode=$2
  valid_name "$name"
  uid=$(id -u "$name")
  case $(uname -s):$mode in
    Linux:loopback)
      for t in iptables ip6tables; do
        sudo -n "$t" -I OUTPUT 1 -m owner --uid-owner "$uid" -j REJECT
        sudo -n "$t" -I OUTPUT 1 -o lo -m owner --uid-owner "$uid" -j ACCEPT
      done
      ;;
    Linux:no-link-local)
      sudo -n iptables -I OUTPUT 1 -d 169.254.0.0/16 -m owner --uid-owner "$uid" -j REJECT
      sudo -n ip6tables -I OUTPUT 1 -d fe80::/10 -m owner --uid-owner "$uid" -j REJECT
      ;;
    Darwin:loopback | Darwin:no-link-local)
      rules=$(mktemp "${RUNNER_TEMP:-/tmp}/pf.XXXXXX")
      cat /etc/pf.conf >"$rules"
      if [ "$mode" = loopback ]; then
        printf 'pass out quick on lo0 all\nblock return out quick proto { tcp udp } all user %s\n' "$uid" >>"$rules"
      else
        printf 'block return out quick proto { tcp udp } to { 169.254.0.0/16 fe80::/10 } user %s\n' "$uid" >>"$rules"
      fi
      sudo -n pfctl -f "$rules" 2>&1 | grep -v 'ALTQ' >&2 || true
      sudo -n pfctl -E 2>&1 | grep -v 'ALTQ' >&2 || true
      sudo -n pfctl -sr 2>/dev/null | grep -q "user = $uid" || die "pf rules for $name are not active"
      rm -f "$rules"
      ;;
    *) die "unknown firewall mode $mode" ;;
  esac
}

trust_ca() {
  pem=$1
  [ -f "$pem" ] || die "no CA file $pem"
  case $(uname -s) in
    Darwin)
      sudo -n security add-trusted-cert -d -r trustRoot -k /Library/Keychains/System.keychain "$pem" \
        || echo "setup-boundary.sh: could not add the proxy CA to the system keychain" >&2
      ;;
    Linux)
      sudo -n cp "$pem" /usr/local/share/ca-certificates/vibeke-herdr-plugins-proxy.crt \
        && sudo -n update-ca-certificates >/dev/null \
        || echo "setup-boundary.sh: could not add the proxy CA to the system store" >&2
      ;;
  esac
}

[ "$#" -ge 1 ] || die "usage: setup-boundary.sh user|seal|firewall|trust-ca …"
cmd=$1
shift
case $cmd in
  user) [ "$#" -eq 1 ] || die "usage: user <name>"; create_user "$1" ;;
  seal) [ "$#" -ge 1 ] || die "usage: seal <dir>..."; seal "$@" ;;
  firewall) [ "$#" -eq 2 ] || die "usage: firewall <name> loopback|no-link-local"; firewall "$1" "$2" ;;
  trust-ca) [ "$#" -eq 1 ] || die "usage: trust-ca <pem>"; trust_ca "$1" ;;
  *) die "unknown command $cmd" ;;
esac
