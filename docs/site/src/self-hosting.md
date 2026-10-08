# Self-host the app and relay

The public services are [app.vibeke.dev](https://app.vibeke.dev), the account-based relay `https://cloud.vibeke.dev`, and the open relay `https://relay.vibeke.dev`.
Use this guide if you want to operate your own services or develop them locally.

The app serves the interface and holds device keys. The relay forwards encrypted traffic.

## Use your own relay

A relay you run needs no Vibeke account. Run it from the published CLI:

```sh
vibeke relay --public-url https://relay.example.com
```

Configure a reverse proxy with HTTPS and WebSocket support for that address.
See the [relay deployment guide](../../../crates/vk-relay/deploy/README.md) for process supervision and deployment details.

To restrict which hosts may register on a private relay, add `--host-token <token>` and set `relay_token` in the host's `gateway.toml`.
Add `--require-tickets` to refuse devices that lack a host-signed ticket.

Pair using your relay and the public browser app:

```sh
vibeke gateway pair \
  --relay https://relay.example.com \
  --app-url https://app.vibeke.dev
```

## Host the browser app

From a checkout of the repository:

```sh
cd web
bun install --frozen-lockfile
bun run build
```

Serve `web/apps/pwa/dist/` from an HTTPS origin. The app uses hash-based routes and requires no application server.
The origin serves trusted JavaScript that can access device keys and decrypted content.

Use `--app-url https://app.example.com` when pairing to select your app origin.
Keep the service worker and manifest on the same origin.

## Local development

Build the browser app, then run this command from the repository root:

```sh
vibeke relay --public-url http://localhost:8787 --app-dir web/apps/pwa/dist
```

With a Vibeke session running, pair in another shell:

```sh
vibeke gateway pair --relay http://localhost:8787 --app-from-relay
```

`--app-from-relay` explicitly trusts the relay origin to serve the app.
Localhost works only for a browser on that computer. A phone needs a reachable HTTPS address.
See the [web development guide](../../../web/README.md) for hot reload and app development.
