#!/bin/sh
# Environment for a plugin that downloads through the verified dependency proxy (spec 18 §7.2).
#
#   proxy-env.sh [--export] <proxy-url> <ca-file>
#
# Prints NAME=VALUE lines for run-boundary.sh --env-file, or `export NAME='VALUE'` lines with
# --export for `eval`. The variables send common tools through the proxy and make them trust
# its CA: curl, git, cargo, npm, bun, node, pip/requests, Go and anything that reads
# SSL_CERT_FILE. npm and cargo read the proxy from HTTPS_PROXY and the CA from
# NODE_EXTRA_CA_CERTS and CARGO_HTTP_CAINFO.
#
# The names are exactly the ones the plugin runner passes through to plugin processes
# (crates/vibeke/tests/it/support/real_plugins.rs): the proxy variables in both cases, NO_PROXY,
# SSL_CERT_FILE, NODE_EXTRA_CA_CERTS, CARGO_HTTP_CAINFO, GIT_SSL_CAINFO, REQUESTS_CA_BUNDLE,
# CURL_CA_BUNDLE, GOPROXY, GOFLAGS and GOSUMDB. run-boundary.sh accepts no other proxy names. A plugin that ignores these variables cannot reach the
# network at all, because the workflow's firewall allows the boundary user only loopback.
set -eu

export_mode=0
if [ "${1:-}" = "--export" ]; then
  export_mode=1
  shift
fi
if [ "$#" -ne 2 ]; then
  echo "usage: proxy-env.sh [--export] <proxy-url> <ca-file>" >&2
  exit 2
fi
proxy=$1
ca=$2
case $proxy in
  http://127.0.0.1:[0-9]*) ;;
  *) echo "proxy-env.sh: the proxy must be http://127.0.0.1:<port>" >&2; exit 2 ;;
esac
case $ca in
  /*) ;;
  *) echo "proxy-env.sh: the CA file must be an absolute path" >&2; exit 2 ;;
esac
case $proxy$ca in
  *"'"*|*' '*) echo "proxy-env.sh: quotes and spaces are not allowed" >&2; exit 2 ;;
esac

emit() {
  if [ "$export_mode" = 1 ]; then
    printf "export %s='%s'\n" "$1" "$2"
  else
    printf '%s=%s\n' "$1" "$2"
  fi
}

no_proxy=127.0.0.1,localhost,::1
for name in HTTPS_PROXY HTTP_PROXY ALL_PROXY https_proxy http_proxy all_proxy; do
  emit "$name" "$proxy"
done
emit NO_PROXY "$no_proxy"
emit no_proxy "$no_proxy"

# Trust stores.
emit SSL_CERT_FILE "$ca"
emit NODE_EXTRA_CA_CERTS "$ca"
emit CURL_CA_BUNDLE "$ca"
emit REQUESTS_CA_BUNDLE "$ca"
emit GIT_SSL_CAINFO "$ca"
emit CARGO_HTTP_CAINFO "$ca"

# Go reads HTTPS_PROXY, and SSL_CERT_FILE on Linux (on macOS it uses the system keychain, where
# the workflow installs the CA). Module downloads use the public module proxy and checksum
# database over HTTPS; `direct` would fetch with git or other protocols. `-modcacherw` keeps the
# module cache removable.
emit GOPROXY https://proxy.golang.org
emit GOSUMDB sum.golang.org
emit GOFLAGS -modcacherw
