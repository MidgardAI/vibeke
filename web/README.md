# Vibeke web apps

The phone/desktop apps of [spec 16](../spec/16-gateway-relay-and-apps.md). Bun workspaces:

| Package | What |
|---|---|
| `packages/core` (`@vibeke/core`) | Noise channel, JSON-RPC client, pairing, multi-host manager, inbox ranking/batching, device-owned Web Push keys (`push.ts`). No DOM, no React. |
| `packages/ui` (`@vibeke/ui`) | Every screen and component (React 19, Tailwind v4, lucide). Depends only on core and a `UiPlatform` handed to `<VibekeApp platform={…}/>`. |
| `apps/pwa` (`@vibeke/pwa`) | The PWA shell: Vite, `vite-plugin-pwa` (injectManifest, `src/sw.ts`), IndexedDB key/host store, WebSocket transport, Web Push, install prompt, speech. |
| `apps/desktop` (`@vibeke/desktop`) | The Electron shell: connections and keys in the main process (safeStorage vault, relay + local Unix-socket transports), menu-bar quick approvals, native notifications, deep links, pop-out pane windows, electron-builder packaging. See [apps/desktop/README.md](apps/desktop/README.md). |

No product code lives in shells: they implement `UiPlatform` and bootstrap.

## Scripts

Run from `web/` with Bun (`mise exec -- bun …` or `~/.bun/bin/bun …`):

```sh
bun install
bun run dev        # Vite dev server for the PWA (http://localhost:5173)
bun run build      # production build → apps/pwa/dist
bun run test       # unit tests (core, ui, pwa)
bun run typecheck  # tsc for every package (incl. the service worker)
bun run icons      # regenerate apps/pwa/public/icons
bun run dev:desktop | build:desktop | dist:desktop | e2e:desktop   # the Electron app
```

## Running the stack locally

You need a running Vibeke server session (`vibeke` or `vibeke server start`), then three processes from the repo root:

```sh
# 1. Relay, also serving the built app (self-hoster mode, spec 16 §9.4)
bun run --cwd web build
cargo run -p vk-relay --bin vibeke-relay -- --public-url http://localhost:8787 --app-dir web/apps/pwa/dist

# 2. Gateway next to the server (dials the relay; no inbound port)
cargo run -p vk-gateway --bin vibeke-gateway -- run --relay http://localhost:8787

# 3. Pair a browser: prints a QR + link, then asks you to confirm the fingerprint
cargo run -p vk-gateway --bin vibeke-gateway -- pair
```

Open the printed link (`http://localhost:8787/#/pair?d=…`), check that the fingerprint shown in the app matches the one the terminal prints, tap **Pair**, and answer `y` in the terminal.

Useful flags: `--session NAME` on `run` picks the server session; `pair --no-confirm` makes the link a bearer invitation (scripted setups); `pair --scope approve|view` pairs a restricted device; `vibeke-gateway devices` / `revoke <id>` manage devices (the app shows "revoked").

### Dev workflow

`bun run dev` serves the app with hot reload on `http://localhost:5173`. The dev server proxies nothing: the app connects straight to the relay URL embedded in the pairing link (`ws://localhost:8787`). Pairing links point at the gateway's `app_url` (default: the relay origin), so either

- start the gateway once with `--app-url http://localhost:5173` (saved to `gateway.toml`), or
- replace `http://localhost:8787` with `http://localhost:5173` in the printed link.

Keys and hosts live in the origin's IndexedDB, so a device paired on `:5173` is a different device from one paired on `:8787`. The service worker (push, offline shell) only runs in the production build (`vite build` + relay `--app-dir`, or `bun run preview`).

## Web Push and iOS

- Push needs a **secure context**: `https://` (or `http://localhost` on the same machine). A phone on your LAN needs TLS in front of the relay (e.g. Caddy) or a tunnel.
- **iOS / iPadOS 16.4+ only deliver Web Push to home-screen apps.** In Safari: Share → *Add to Home Screen*, open Vibeke from the icon, then Settings → Alerts → **Turn on**. The permission prompt must come from that tap; the app shows an install guide instead of the button while it runs in a Safari tab.
- The device owns one P-256 VAPID key pair (IndexedDB), subscribes once with its public key, and sends `{subscription, vapid_private}` to **every** paired host (`push.subscribe`). The subscription is re-checked on every app start and re-sent when the endpoint changed. Forgetting a host rotates the key and re-sends to the remaining hosts.
- WebKit revokes permission for pushes that show nothing, so the service worker shows a notification for every push. Notifications are one per host (`tag = vibeke:<host id>`); on every foreground the app closes those whose host has nothing open and updates the app badge.
- Hosts push only while the device has no visible lease (app in the background), respect DND and the per-device privacy level (Settings → Alerts), and the push service allow-list on the gateway (`*.push.apple.com`, FCM, Mozilla, WNS).

## Code trust

The JavaScript that runs the app holds the device key and sees decrypted content, so the origin serving it is trusted (spec 16 §9.4). Settings → About shows that origin and the build hash (`VIBEKE_BUILD_HASH` overrides the git hash at build time).
