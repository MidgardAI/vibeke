# vibeke-relay

The relay lets phones and desktops reach a Vibeke host that has no inbound port (spec 16 §6). It
only forwards end-to-end encrypted WebSocket messages; it never sees keys or content. A relay you run needs no
account. By default anyone can register a host and limits keep abuse cheap; use `--host-token` to
restrict hosts (private relay) and `--require-tickets` to refuse devices without a host-signed
ticket (spec 16 §6.6).

## Run locally

```sh
cargo run -p vk-relay --bin vibeke-relay -- --public-url http://127.0.0.1:8787
```

## Deploy (VPS)

1. Build a static binary: `cargo zigbuild -p vk-relay --release --target x86_64-unknown-linux-musl`.
2. Copy it to `/usr/local/bin/vibeke-relay`, install `vibeke-relay.service`, set your domain in
   `--public-url`, `systemctl enable --now vibeke-relay`.
3. Put Caddy in front with the `Caddyfile` (automatic TLS, WebSocket upgrade).
4. Optional: serve the web app from the relay with `--app-dir` (only if you are also the app
   publisher, spec 16 §9.4).

`--public-url` must be exactly what gateways dial (scheme + host + port): hosts sign over it, so a
mismatch fails host authentication with close code 4401.

## Flags

| Flag | Default | |
|---|---|---|
| `--listen` | `127.0.0.1:8787` | bind address |
| `--public-url` | required, repeatable | origins hosts sign over |
| `--app-dir` | none | static web app |
| `--host-token` | none | require one of these tokens from hosts |
| `--require-tickets` | off | refuse devices without a host-signed ticket |
| `--account-url` | none | account URL shown on `/v1/status` |
| `--trust-proxy` | off | client IP from `X-Forwarded-For` |
| `--log-ip-raw` | off | log raw client IPs instead of daily-keyed hashes |
| `--conn-bytes-per-sec` | 1 MiB | per connection and direction |
| `--max-hosts` / `--max-conns` | 10 000 / 50 000 | global caps |
