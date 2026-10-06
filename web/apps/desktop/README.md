# Vibeke desktop (`@vibeke/desktop`)

The Electron app (spec 16 §16) over the shared packages: `@vibeke/core` (channel, pairing, hosts)
and `@vibeke/ui` (every screen). macOS first; Linux and Windows build and run.

```sh
# from web/
bun install                 # downloads Electron (trustedDependencies)
bun run dev:desktop         # Vite dev server (HMR for the UI) + Electron; restart for main-process changes
bun run build:desktop       # out/{main,preload,renderer}
bun run dist:desktop        # installers for this OS into apps/desktop/dist
bun run test:desktop        # desktop unit tests (headless; also part of `bun run test`)
bun run e2e:desktop         # Playwright for Electron (smoke, real gateway, memory budget)
bun run dist:desktop -- --dir   # unsigned/ad-hoc app directory (CSC_IDENTITY_AUTO_DISCOVERY=false)

# from web/apps/desktop/
bun run dist:dir            # app directory only, signed ad hoc (runs on this machine)
bun run dist:mac | dist:linux | dist:win
bun run test                # unit tests (also part of `bun run test` at web/)
bun run icons               # regenerate build/ icons (pure code, committed)
```

## Architecture

```
main process (Node)                                    renderer (sandboxed, per window)
  vault.ts     safeStorage-encrypted keys + hosts        @vibeke/ui <VibekeApp surface=…>
  engine.ts    core HostManager: one connection          platform.ts  UiPlatform: HostEngine proxy,
               per host, shared by all windows                        no sockets, no keys
  transport.ts RelayTransport (wss) / LocalTransport     remote.ts    RemoteManager: host-state
               (WebSocket over <gateway dir>/gateway.sock)            patches + request over IPC
  notifier.ts  native notifications (alerts.ts)          extensions   "Connect to this Mac",
  windows.ts   main / quick popover / pane windows                    Desktop settings, commands
  tray.ts menu.ts ipc.ts validate.ts deeplink.ts
  local-pair.ts protocol.ts updater.ts                   preload: window.vibeke = {invoke, on}
```

- **Connections live in the main process.** Core's `HostManager` runs there with the device key
  from the vault, so closing every window keeps the app connected (menu bar / tray), notifications
  and the tray count work with no window open, and the main window, the popover and every pane
  window share one connection per host. Renderers receive host state as patches (only while
  visible; a hidden window catches up before it is shown) and call the app API through
  `vk:host.request`. The device private key never enters a renderer. (Spec §9.3 sketches the
  renderer running core over bridged sockets; with several windows that would mean one connection,
  one dashboard refetch loop and one notifier per window, and nothing would survive a closed window.)
- **Transports.** A host record's `relay` is `wss://…` or `local:<socket path>` (from
  `vibeke gateway pair --local`); `parseConnectUrl` (core) picks the transport. Both carry the
  same Noise channel; the gateway authenticates the device exactly as over a relay.
- **Keys.** `<userData>/vault.bin` (0600, atomic temp+fsync+rename) holds
  `safeStorage.encryptString(JSON{keys, hosts})`. All operations are serialized, so
  `getOrCreate` is atomic; the single-instance lock makes this process the only writer. Linux's
  `basic_text` backend is refused with instructions; an undecryptable vault is reported, never
  replaced (delete `vault.bin` to start over and pair again).
- **Security.** `contextIsolation`, `sandbox` (also `app.enableSandbox()`), no `nodeIntegration`,
  no webviews. The bundle is served from `app://vibeke` with a strict CSP header (no inline
  script, `connect-src 'self'`); navigation and `window.open` are denied (http(s) links open in
  the browser after validation). Every IPC handler checks that the sender is the main frame of one
  of our windows at the app origin, then validates every argument (`validate.ts`; renderers may
  only call app-API methods on an allow-list: no `hello`, `events.subscribe`, `push.*` …).
  Renderers cannot choose what main executes or where updates come from: the `vibeke` executable
  is only set through a native open panel shown by main (`vk:local.choose-binary`, validated
  there), and the update feed is the packaged `app-update.yml`.
  Permissions: microphone (audio only, for voice input) and clipboard writes; everything else is
  denied. Packaged builds flip Electron fuses (no `ELECTRON_RUN_AS_NODE`, no `--inspect`, asar
  integrity, app only from asar).

## Desktop features

- **Connect to this Mac**: the pairing screen's first card runs `vibeke gateway pair --local`,
  then pairs over the gateway's Unix socket. The CLI is the one chosen in Settings → Desktop
  (a broken choice is reported, never skipped), else `$VIBEKE_BIN`, else the usual install
  locations (`~/.local/bin`, `/opt/homebrew/bin`, `/usr/local/bin`, `~/.cargo/bin`, `/usr/bin`),
  else absolute `PATH` entries. Every candidate is canonicalized and must be a regular executable
  owned by you or root, not writable by others, in a directory others cannot write to; relative
  `PATH` entries are ignored, and a CLI found only somewhere unusual on `PATH` must be confirmed in
  the native picker (which also checks that `--version` prints a Vibeke version). If the gateway is
  not running it offers **Start gateway** (`vibeke gateway run`, detached, log in
  `<userData>/logs/gateway.log`); if the gateway waits for the server it says so.
- **Pairing links**: paste on the pairing screen, or open `vibeke://pair?d=…` /
  `https://<app>/#/pair?d=…` (also `vibeke://inbox`, `vibeke://i/<host>/<id>`,
  `vibeke://h/<host>/p/<pane>`). A second launch forwards its link to the running app.
- **Menu-bar quick approvals**: tray icon (template image on macOS) with the open-interaction count
  as its title and on the Dock badge. Click it or press **⌥⌘V** (configurable in Settings →
  Desktop, recorded from the keyboard) for the popover: the inbox cards and batches,
  `j`/`k`/`a`/`d`/`A`/`Enter`, `Esc` closes, clicking elsewhere dismisses it.
- **Notifications** from the main process: newly opened approvals / questions / plan reviews,
  stopped agents (error, rate limited) and, if the host's "finished" toggle is on, finished agents
  (30 s debounce). One notification per host, merged ("3 agents need you"), at each host's privacy
  level (`full` is redacted like the gateway's pushes), suppressed while an app window is focused,
  host-wide DND respected; nothing for what was already open at startup. Nothing is shown for a
  host until its preferences are known (one shared `prefs.get` per host; a failed refetch keeps
  the last known ones), and what is shown is re-read after that wait. Resolutions update the
  merged notification quietly. Click opens the card; on
  macOS **Approve…** opens the popover on that card with a confirm, **Open** opens the app.
- **Keyboard-first** (in `@vibeke/ui`, so the PWA has it too): `⌘K` palette (panes, workspaces,
  hosts, open interactions, new agent, share, hand off, pair, settings, theme…), `j`/`k`, `a`/`d`/`A`,
  `Enter`, `Esc`, `⌘1–4`, `/`, `?` cheat sheet. Actions press the same buttons a click would, so
  high/unknown-risk and "allow always" still ask to confirm; the first key on an unselected list
  only selects. The selection follows focus and clicks, a held key never answers twice, and when
  the selected card goes away its successor is only highlighted (the next key confirms it).
  The palette and every sheet are modal dialogs: focus stays inside, the rest is inert, and focus
  returns to the control that opened them.
- **Windows**: hidden-inset title bar with traffic lights, vibrancy sidebar (wide layout) and
  popover, system accent colour, light/dark following the OS (or the app's theme setting). Pop a
  pane out (`⇧⌘O` or the pane header button): a pane window stays on that pane (no previous /
  next; other routes open in the main window). Window positions are remembered. Closing the main
  window keeps the app running; quit with `⌘Q` / tray → Quit. Hidden windows do no display work
  (timers pause); the quick popover's renderer is released after 60 s hidden and the main
  window's after 10 min (both are recreated on demand, ~0.7 s).
- **Settings → Desktop**: quick-approvals shortcut, notifications, open at login (menu bar only,
  no window), show in Dock (macOS), `vibeke` command (Choose… opens the native picker).
- **Updates**: `electron-updater` with a generic feed baked in at packaging time
  (`VIBEKE_UPDATE_URL`, https only → `Contents/Resources/app-update.yml`). Packaged builds without
  that file never load the updater (it is a separate bundle, `out/main/updater-impl.cjs`);
  nothing at run time can change the feed.

## Packaging and signing

`electron-builder.config.cjs`: macOS dmg + zip (arm64, x64), Linux AppImage + deb, Windows nsis.
Secrets come from the environment only:

| What | Environment |
|---|---|
| macOS signing | `CSC_LINK` (+ `CSC_KEY_PASSWORD`), or `CSC_NAME` / `VIBEKE_MAC_SIGN=1` for a keychain identity |
| Notarization | `APPLE_API_KEY` + `APPLE_API_KEY_ID` + `APPLE_API_ISSUER`, or `APPLE_ID` + `APPLE_APP_SPECIFIC_PASSWORD` + `APPLE_TEAM_ID`, or `APPLE_KEYCHAIN_PROFILE` |
| Windows signing | `WIN_CSC_LINK` (+ `WIN_CSC_KEY_PASSWORD`) |
| Update feed | `VIBEKE_UPDATE_URL` |

Without a signing identity the macOS app is signed ad hoc (Apple silicon refuses unsigned code, and
applying fuses invalidates Electron's own signature) and the hardened runtime is off; such a build
runs on the machine that built it.

## Tests

- `test/` (Bun, headless: `bun run test:desktop`): IPC validators (renderers cannot set the CLI
  path or update feed), the vault with a fake `safeStorage` (encryption at rest, 0600, atomic
  get-or-create, `basic_text` refusal, corrupt vault), deep links, transport selection,
  notification content and privacy levels, the alert tracker (baseline, merge, stopped/finished),
  the notifier with fakes (fails closed until prefs are known, one `prefs.get` per host, restrictive
  prefs kept on failure, re-validation after awaiting, finished notifications tracked by host,
  quiet content updates), `pair --local` output parsing, CLI discovery and trust checks (relative
  PATH, ownership, writability, symlinks, unexpected locations), the packaged update feed, window
  bounds / popover placement, app:// path traversal, shortcut recording.
- `e2e/` (Playwright `_electron`, launches `out/`; skipped without a display; `bun run e2e:desktop`
  builds first):
  - `smoke.e2e.ts`: launch → pairing screen, sandboxed renderer (no `require`/`process`, bridge is
    `{invoke, on}`), the settings IPC refuses `vibekePath`/`updateFeed`, `⌘K` palette → Settings,
    `?` cheat sheet, "Connect" without a CLI explains, quit ends the process; a `vibeke://inbox`
    deep link given at launch arrives after the renderer's ready handshake.
  - `local-gateway.e2e.ts`: starts a real `vibeke` server + gateway in temp dirs (temp `HOME`,
    XDG dirs, short `VIBEKE_RUNTIME_DIR`, own `VIBEKE_GATEWAY_DIR`), clicks "Connect to this Mac",
    opens two approvals from two panes with `vibeke hook claude PermissionRequest`, checks the
    palette is modal (focus trapped, `#root` inert, focus restored), the quick popover, keyboard
    answers (focus moves the selection, a held `a` answers once, the auto-advanced successor needs
    a fresh key), the hooks receive `allow`, a popped-out pane window has no previous/next or Lock.
    Takes light/dark screenshots of the inbox, panes, settings, palette, popover and pane window
    into `test-results/` after animations settle (page screenshots cannot see native vibrancy, so
    a stand-in sidebar material is painted for the capture).
  - `memory.e2e.ts`: three isolated servers + gateways, paired over their local sockets; prints
    working set and physical footprint per process with the main window, the popover, and menu
    bar only (after hidden renderers are released), and asserts the 250 MB budget on the
    footprint. `VIBEKE_E2E_MEMORY_BUDGET=0` only reports.
  - The CLI comes from `VIBEKE_BIN`, else `VIBEKE_TEST_BIN`, else `target/debug/vibeke`
    (`cargo build -p vibeke`). All processes started (servers, gateways, pane holders) are stopped
    and their temp dirs removed afterwards.
- Both use `--use-mock-keychain` so `safeStorage` never prompts. `VIBEKE_USER_DATA` gives every
  run its own profile. `VIBEKE_POPOVER_TTL_MS` / `VIBEKE_MAIN_TTL_MS` shorten the renderer release
  delays for tests.

## Files

- `vault.bin`, `settings.json`, `window-state.json`, `logs/` in the user-data dir
  (`~/Library/Application Support/Vibeke` on macOS). `VIBEKE_USER_DATA` overrides it.
