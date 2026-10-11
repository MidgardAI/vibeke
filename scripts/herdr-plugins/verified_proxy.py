"""mitmproxy addon for the `verified` dependency mode of the real Herdr plugin harness (spec 18 §7.2).

Run by the trusted runner user, outside the execution boundary:

    mitmdump --listen-host 127.0.0.1 --listen-port 8899 --set confdir=<dir> \
        -s scripts/herdr-plugins/verified_proxy.py \
        --set hp_mode=record|verify --set hp_cache=<dir> --set hp_log=<dir> [--set hp_allow=<file>]

Both modes forward only GET and HEAD requests, and only to public addresses on ports 80 and 443
(`hp_netfilter.py`). The proxy resolves each upstream host name itself, refuses it when any of
its addresses is loopback, private, link-local (cloud metadata services), CGNAT, multicast or
unspecified, and then connects to the checked address, so a second DNS answer cannot redirect the
connection (DNS rebinding). TLS still verifies the certificate against the host name.

`record` forwards every such request, stores each response body in a content-addressed cache
(`<cache>/<sha[:2]>/<sha>` plus a `.meta.json` file) and appends `{url, sha256, status}` lines
to `<log>/record.jsonl`. `scripts/herdr-plugins-record.ts` merges those lines into the lock.

`verify` first refuses every request whose URL is not in the allowlist. It then serves only
responses whose sha256 is in the allowlist file (`{"dependencies":
[{url, sha256}]}`, written by the workflow from the lock). A known URL is answered from the cache
when the cached body still has a listed hash. Other responses are fetched in full, hashed and
replaced by a 403 before any byte reaches the client. Redirects pass only for URLs listed in the
lock. Every refusal is logged to `<log>/blocked.jsonl`. Raw TCP, UDP and WebSocket traffic is
refused: plugins that need other protocols or pin certificates stay `unpinned` (18 §15).

Bodies are hashed as the client receives them. The proxy drops `Accept-Encoding` so servers send
the same bytes in both modes.
"""

import asyncio
import hashlib
import json
import logging
import os
import socket
import tempfile
import threading

from mitmproxy import ctx, http

# mitmproxy puts this script's directory on sys.path while it loads the script.
from hp_netfilter import port_refusal, request_refusal, resolved_target

REDIRECTS = {301, 302, 303, 307, 308}
KEPT_HEADERS = ("content-type", "content-encoding", "location", "content-disposition")


class VerifiedProxy:
    def __init__(self):
        self.lock = threading.Lock()
        self.allowed_sha = set()
        self.url_shas = {}

    def load(self, loader):
        loader.add_option("hp_mode", str, "verify", "record or verify")
        loader.add_option("hp_cache", str, "", "content-addressed cache directory")
        loader.add_option("hp_log", str, "", "directory for record.jsonl and blocked.jsonl")
        loader.add_option("hp_allow", str, "", "allowlist JSON written from the lock")

    def configure(self, updated):
        if ctx.options.hp_mode not in ("record", "verify"):
            raise ValueError("hp_mode must be record or verify")
        if not ctx.options.hp_log:
            raise ValueError("hp_log is required")
        os.makedirs(ctx.options.hp_log, exist_ok=True)
        if ctx.options.hp_cache:
            os.makedirs(ctx.options.hp_cache, exist_ok=True)
        if ctx.options.hp_mode == "verify":
            if not ctx.options.hp_allow:
                raise ValueError("hp_allow is required in verify mode")
            with open(ctx.options.hp_allow, encoding="utf-8") as f:
                deps = json.load(f).get("dependencies", [])
            self.allowed_sha = {d["sha256"] for d in deps}
            self.url_shas = {}
            for d in deps:
                self.url_shas.setdefault(d["url"], []).append(d["sha256"])

    # Logging and cache helpers.

    def _append(self, name, record):
        with self.lock:
            with open(os.path.join(ctx.options.hp_log, name), "a", encoding="utf-8") as f:
                f.write(json.dumps(record, sort_keys=True) + "\n")

    def _blob_path(self, sha):
        return os.path.join(ctx.options.hp_cache, sha[:2], sha)

    def _store(self, sha, body, response):
        if not ctx.options.hp_cache:
            return
        path = self._blob_path(sha)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        meta = {
            "status": response.status_code,
            "headers": {k: response.headers[k] for k in KEPT_HEADERS if k in response.headers},
        }
        for target, data in ((path, body), (path + ".meta.json", json.dumps(meta).encode())):
            fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path))
            with os.fdopen(fd, "wb") as f:
                f.write(data)
            os.replace(tmp, target)

    def _cached(self, url):
        """The cached response for a listed URL, if a listed hash is in the cache and intact."""
        if not ctx.options.hp_cache:
            return None
        for sha in self.url_shas.get(url, []):
            path = self._blob_path(sha)
            try:
                with open(path, "rb") as f:
                    body = f.read()
                with open(path + ".meta.json", encoding="utf-8") as f:
                    meta = json.load(f)
            except OSError:
                continue
            if hashlib.sha256(body).hexdigest() != sha:
                continue
            headers = {str(k): str(v) for k, v in meta.get("headers", {}).items()}
            return http.Response.make(int(meta.get("status", 200)), body, headers)
        return None

    def _refuse(self, flow, why, sha=None):
        url = flow.request.pretty_url
        self._append("blocked.jsonl", {"url": url, "sha256": sha, "reason": why})
        logging.warning("verified proxy refused %s: %s", url, why)
        flow.response = http.Response.make(
            403,
            f"vibeke verified dependency mode refused this download: {why}\n".encode(),
            {"content-type": "text/plain"},
        )

    # mitmproxy hooks.

    def http_connect(self, flow: http.HTTPFlow):
        # A CONNECT tunnel to another port is refused before mitmproxy opens it. `server_connect`
        # checks the address of every connection again.
        why = port_refusal(flow.request.port)
        if why:
            flow.metadata["hp_refused"] = True
            self._refuse(flow, why)

    def request(self, flow: http.HTTPFlow):
        flow.request.headers.pop("accept-encoding", None)
        why = request_refusal(ctx.options.hp_mode, flow.request.method, flow.request.pretty_url, self.url_shas)
        if why is None:
            why = port_refusal(flow.request.port)
        if why:
            # Setting a response here means mitmproxy never forwards the request.
            flow.metadata["hp_refused"] = True
            self._refuse(flow, why)
            return
        if ctx.options.hp_mode == "verify":
            cached = self._cached(flow.request.pretty_url)
            if cached is not None:
                flow.response = cached
                flow.metadata["hp_from_cache"] = True

    async def server_connect(self, data):
        """Checks every upstream connection before mitmproxy opens it, and pins its address."""
        server = data.server
        host, port = server.address
        if server.transport_protocol != "tcp" or server.via is not None:
            why = "only direct TCP connections are allowed"
        else:
            try:
                infos = await asyncio.get_running_loop().getaddrinfo(host, port, type=socket.SOCK_STREAM)
                addresses = [info[4][0] for info in infos if info[0] in (socket.AF_INET, socket.AF_INET6)]
            except (OSError, UnicodeError) as e:
                addresses = []
                logging.warning("verified proxy could not resolve %s: %s", host, e)
            target, why = resolved_target(addresses, port)
            if target is not None:
                # TLS must still check the certificate against the name, not the address.
                if server.sni is None and target != host:
                    server.sni = host
                server.address = (target, port)
                return
        self._append("blocked.jsonl", {"url": f"tcp://{host}:{port}", "sha256": None, "reason": why})
        logging.warning("verified proxy refused a connection to %s:%s: %s", host, port, why)
        server.error = f"vibeke dependency proxy: {why}"

    def responseheaders(self, flow: http.HTTPFlow):
        # Never stream, whatever `stream_large_bodies` says: the whole body is hashed before the
        # client sees any of it. Script addons run after the built-in streaming addon.
        flow.response.stream = False

    def response(self, flow: http.HTTPFlow):
        if flow.metadata.get("hp_from_cache") or flow.metadata.get("hp_refused"):
            return
        body = flow.response.raw_content or b""
        sha = hashlib.sha256(body).hexdigest()
        url = flow.request.pretty_url
        status = flow.response.status_code
        if ctx.options.hp_mode == "record":
            self._store(sha, body, flow.response)
            self._append("record.jsonl", {"url": url, "sha256": sha, "status": status})
            return
        if status in REDIRECTS:
            allowed = url in self.url_shas
            why = "redirect from a URL that is not in the lock"
        else:
            allowed = sha in self.allowed_sha
            why = "response hash is not in the lock"
        if not allowed:
            flow.metadata["hp_refused"] = True
            self._refuse(flow, why, sha)

    def error(self, flow: http.HTTPFlow):
        self._append("blocked.jsonl", {"url": flow.request.pretty_url, "sha256": None, "reason": "upstream error"})

    def websocket_start(self, flow: http.HTTPFlow):
        self._append("blocked.jsonl", {"url": flow.request.pretty_url, "sha256": None, "reason": "websocket"})
        flow.kill()

    def tcp_start(self, flow):
        self._append("blocked.jsonl", {"url": f"tcp://{flow.server_conn.address}", "sha256": None, "reason": "raw TCP"})
        flow.kill()

    def tls_failed_client(self, data):
        # Usually a client that pins its certificates or does not trust the proxy CA.
        sni = getattr(data.conn, "sni", None) or "unknown"
        self._append("blocked.jsonl", {"url": f"tls://{sni}", "sha256": None, "reason": "client TLS handshake failed"})

    def udp_start(self, flow):
        self._append("blocked.jsonl", {"url": f"udp://{flow.server_conn.address}", "sha256": None, "reason": "raw UDP"})
        flow.kill()


addons = [VerifiedProxy()]
