# Vibeke browser app

Production: https://app.vibeke.dev. Vercel project: `your-team/vibeke-app`.
This project serves the static PWA. The public relay runs separately at `https://relay.vibeke.dev`. Hosts sign in with `vibeke login` before pairing through it.
The marketing site and documentation use the `your-team/vibeke-dev` project at `https://vibeke.dev`.

## Build and deploy

Install dependencies from `web/` with `bun install --frozen-lockfile`.
From `web/apps/pwa/`:

```sh
bun run typecheck
bun run test
vercel link --yes --project vibeke-app --scope your-team
bun run build:vercel
vercel deploy --prebuilt --prod --scope your-team
```

The build records the Git commit in the app's About screen. Deploy from a committed checkout.
`build:vercel` writes the Vercel Build Output API files without uploading source or requiring remote workspace dependencies.
The service worker, HTML, and manifest revalidate. Hashed assets use immutable caching.
Hash-based routes keep pairing invitations in the browser fragment.

Configure the `app` DNS record using the target shown by `vercel domains inspect app.vibeke.dev --scope your-team`.
The domain must also be assigned to `vibeke-app` with Vercel deployment protection disabled for the public production app.

## Pair a host

With a Vibeke session running:

```sh
vibeke gateway pair --relay https://relay.vibeke.dev --app-url https://app.vibeke.dev
```

Open the generated link and confirm the device fingerprint. See the [public guide](https://vibeke.dev/docs/mobile).
Before publishing, verify the app shell, pairing route, manifest, service worker activation, and an encrypted relay connection.

Run `bun scripts/check-app.ts https://app.vibeke.dev` from `web/apps/site/` to check desktop/mobile rendering and the offline shell.
Run `bun web/packages/core/scripts/check-relay.ts wss://relay.vibeke.dev` from the repository root for an encrypted transport check.

## Experimental browser TUI

This branch can run the Rust TUI in WebAssembly. The host still owns its terminals and
processes. The browser handles the TUI layout, menus, keybindings, and screen updates.

Build the native host from this branch. Install the pinned Rust toolchain from `mise.toml`,
the `wasm32-unknown-unknown` target, and `wasm-bindgen-cli` version `0.2.129`:

```sh
mise exec -- rustup target add wasm32-unknown-unknown
mise exec -- cargo install wasm-bindgen-cli --version 0.2.129 --locked
mise exec -- cargo build -p vibeke
```

From `web/`, run:

```sh
bun install --frozen-lockfile
bun run dev:tui
```

`WASM_BINDGEN` can point to a downloaded CLI executable of the same version.
Pair through a relay as described above. For local development, set the pairing command's
`--app-url` to `http://localhost:5173`. Use the binary built from this branch for the host.
Open **Settings → Hosts → Open TUI** on a paired host with full access.
The route is `#/tui/<host-id>`.

`bun run build:pwa:tui` builds the PWA with this experiment enabled. Ordinary builds leave
the entry point disabled. WASM assets are generated under `public/tui/` and are not committed.
They have content-based URLs. They load online and are outside the offline app cache.

The TUI shares the paired device's encrypted connection. The relay cannot read its screen
or input. View-only devices, approval-only devices, shares, and peers cannot attach.
The browser and host must use the same render protocol version. Closing the TUI releases
its render connection. Closing the tab leaves host processes running.

The browser version currently uses the default TUI configuration and one host per view.
Browser shortcuts can take priority over terminal shortcuts. The toolbar opens the command
palette and pastes text through the browser clipboard permission prompt.
Text pastes are limited to 128 KiB. Native integrations such as local file uploads, external
editors, local shell shortcuts, local configuration writes, and CLI self-update are unavailable.
Security confirmations that require a local TUI still require that local client.
Pending task operations use the tab's session storage for reconnect and reload recovery.
Do not clear that storage while an operation has an uncertain result.

After building the host and WASM module, run this check from `web/`:

```sh
bun apps/site/scripts/check-wasm-tui.ts
```

It needs Playwright Chromium (`bun apps/site/node_modules/@playwright/test/cli.js install chromium`).
It starts an isolated host and relay under `/tmp`, pairs a fresh browser, and checks rendering,
shell input, Unicode, the command palette, resize, reconnect, reload, and revocation.
It stops its processes when done. It does not use an existing Vibeke session.
To check the packaged PWA and its security headers, run from `web/`:

```sh
VIBEKE_WASM_TUI=1 bun run --cwd apps/pwa build:vercel
VIBEKE_TUI_PRODUCTION=1 bun apps/site/scripts/check-wasm-tui.ts
```
