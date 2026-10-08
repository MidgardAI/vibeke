#!/bin/sh
# Run as root on the dedicated Ubuntu VM, over SSH. Arguments contain no secrets.
set -eu
release=$1
bundle=$2
case "$release" in ''|*[!a-f0-9]*) echo 'Invalid release ID' >&2; exit 1 ;; esac
[ "${#release}" = 20 ]
[ "$bundle" = "/root/vibeke-$release.tar.gz" ]
base=/opt/vibeke-relay
mkdir -p "$base/releases"
chmod 755 "$base" "$base/releases"
# Only one install may switch the active release at a time, including CI runs.
exec 9>"$base/deploy.lock"
flock -w 600 9
previous=$(readlink "$base/current" || true)
stage=$(mktemp -d "$base/releases/.stage-XXXXXX")
trap 'rm -rf "$stage"; rm -f "$bundle"' EXIT
tar --no-same-owner -xzf "$bundle" -C "$stage"
(cd "$stage" && sha256sum --check SHA256SUMS)
printf '%s\n' "$release" > "$stage/RELEASE"
chmod -R a+rX "$stage"
if [ ! -d "$base/releases/$release" ]; then
  mv "$stage" "$base/releases/$release"
fi
target="$base/releases/$release"
if ! command -v caddy >/dev/null 2>&1 || ! command -v curl >/dev/null 2>&1; then
  export DEBIAN_FRONTEND=noninteractive
  apt-get update -qq
  apt-get install -y --no-install-recommends caddy curl ca-certificates
fi
caddy validate --adapter caddyfile --config "$target/Caddyfile"
if [ "$previous" = "$target" ] && systemctl is-active --quiet vibeke-relay && systemctl is-active --quiet caddy &&
   [ "$(curl -fsS --max-time 5 http://127.0.0.1:8787/healthz)" = ok ]; then
  echo "Release $release already runs; no restart needed."
  exit 0
fi
switch() {
  ln -sfn "$1" "$base/next" &&
  mv -Tf "$base/next" "$base/current" &&
  ln -sfn "$base/current/vibeke-relay.service" /etc/systemd/system/vibeke-relay.service &&
  ln -sfn "$base/current/Caddyfile" /etc/caddy/Caddyfile &&
  systemctl daemon-reload
}
rollback() {
  echo 'Install failed; restoring the previous release.' >&2
  if [ -n "$previous" ]; then
    switch "$previous"
    systemctl restart vibeke-relay
    systemctl reload-or-restart caddy
  else
    systemctl stop vibeke-relay || true
  fi
}
if ! switch "$target" || ! systemctl enable vibeke-relay caddy || ! systemctl restart vibeke-relay; then
  rollback
  exit 1
fi
attempt=0
until [ "$(curl -fsS --max-time 2 http://127.0.0.1:8787/healthz || true)" = ok ]; do
  attempt=$((attempt + 1))
  if [ "$attempt" -ge 20 ]; then rollback; exit 1; fi
  sleep 1
done
if ! systemctl reload-or-restart caddy; then rollback; exit 1; fi
echo "Release $release installed; local relay health passed."
