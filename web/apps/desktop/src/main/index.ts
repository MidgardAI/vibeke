// Vibeke desktop: main process bootstrap (spec 16 §16). Owns keys (vault), connections (engine),
// notifications, tray, global shortcut, deep links, windows, menus and updates. Renderers are the
// shared UI behind a narrow preload bridge.

import { randomBytes } from 'node:crypto';
import { existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { hostname } from 'node:os';
import { join } from 'node:path';
import {
  BrowserWindow,
  app,
  dialog,
  globalShortcut,
  ipcMain,
  Menu,
  nativeTheme,
  Notification,
  powerMonitor,
  safeStorage,
  session,
  shell,
  systemPreferences,
  type WebContents,
} from 'electron';
import { systemClock, type Lifecycle, type Platform } from '@vibeke/core';
import { EVENT, INVOKE, type BootInfo, type ChooseVibekeResult, type DesktopSettings, type LocalConnectResult, type WireResult } from '../shared/contract';
import { EventSubscriptions, forwardableEvent, type HostEventPayload } from '../shared/host-events';
import { deepLinkFromArgv, deepLinkToHash, PROTOCOL } from './deeplink';
import { Engine } from './engine';
import { registerIpc } from './ipc';
import { checkExecutable, discoverVibeke, gatewaySocket, runPairLocal, socketAlive, startGateway, vibekeVersion, waitForSocket } from './local-pair';
import { buildMenu } from './menu';
import { Notifier } from './notifier';
import { APP_ORIGIN, CSP, handleAppProtocol, registerScheme } from './protocol';
import { loadSettings, writeJson } from './store';
import { connectNode } from './transport';
import { AppTray } from './tray';
import { startUpdates } from './updater';
import { externalUrl, isTrustedUrl } from './validate';
import { Vault } from './vault';
import { Windows } from './windows';

declare const __APP_VERSION__: string;
declare const __BUILD_HASH__: string;

const log = (msg: string) => console.log(`[vibeke] ${msg}`);

// ---- before ready ---------------------------------------------------------------------------

// Tests and side-by-side profiles: an isolated user-data dir.
if (process.env.VIBEKE_USER_DATA) app.setPath('userData', process.env.VIBEKE_USER_DATA);
app.setName('Vibeke');
app.enableSandbox();
registerScheme();

const devServer = !app.isPackaged ? process.env.VITE_DEV_SERVER_URL : undefined;
const trustedOrigins = (): string[] => (devServer ? [APP_ORIGIN, new URL(devServer).origin] : [APP_ORIGIN]);
const rendererUrl = (surface: string, hash = ''): string => `${devServer ? devServer.replace(/\/$/, '') : APP_ORIGIN}/index.html?surface=${surface}${hash}`;

if (!app.requestSingleInstanceLock()) {
  app.quit();
  process.exit(0);
}

// Deep links arriving before the app is ready are queued.
let pendingLink: string | null = deepLinkFromArgv(process.argv);
let ready = false;
const openLink = (raw: string) => {
  const hash = deepLinkToHash(raw);
  if (!hash) return log(`ignored deep link`);
  if (!ready) pendingLink = raw;
  else windows.showMain(hash);
};
app.on('open-url', (e, url) => {
  e.preventDefault();
  openLink(url);
});
app.on('second-instance', (_e, argv) => {
  const link = deepLinkFromArgv(argv);
  if (link) openLink(link);
  else if (ready) windows.showMain();
});

if (process.defaultApp && process.argv[1]) {
  // `electron .` in development: register the protocol with our script path.
  app.setAsDefaultProtocolClient(PROTOCOL, process.execPath, [join(process.cwd(), process.argv[1])]);
} else {
  app.setAsDefaultProtocolClient(PROTOCOL);
}

// ---- state ----------------------------------------------------------------------------------

const userData = app.getPath('userData');
// The bundle's own directory (bundlers inline `__dirname` as the source path): out/ under the app.
const outDir = join(app.getAppPath(), 'out');
const settingsFile = join(userData, 'settings.json');
let settings: DesktopSettings = loadSettings(settingsFile);

const vault = new Vault(userData, safeStorage);

const visible = new Set<() => void>();
const hidden = new Set<() => void>();
let wasVisible = false;
const lifecycle: Lifecycle = {
  onVisible: (cb) => (visible.add(cb), () => visible.delete(cb)),
  onHidden: (cb) => (hidden.add(cb), () => hidden.delete(cb)),
  isVisible: () => windows?.anyVisible() ?? false,
};

const platformName = process.platform === 'darwin' ? 'macOS' : process.platform === 'win32' ? 'Windows' : 'Linux';

const platform: Platform = {
  keystore: vault.keystore,
  connect: connectNode,
  clock: systemClock,
  random: (n) => new Uint8Array(randomBytes(n)),
  lifecycle,
  platformName,
};

const version = typeof __APP_VERSION__ === 'string' ? __APP_VERSION__ : app.getVersion();
const engine = new Engine({ platform, hostStore: vault.hosts, client: { client: 'vibeke-desktop', version } });

function deviceName(): string {
  let h = hostname().replace(/\.local$/i, '');
  h = h.replace(/[-_]+/g, ' ').trim();
  return (h || platformName).slice(0, 64);
}

// ---- windows, tray, notifications -----------------------------------------------------------

/** Windows that missed host updates while hidden (flushed before they show). Weak: a destroyed
 * window's webContents must not be kept alive by this set. */
const stale = new WeakSet<WebContents>();
const sentVisibility = new WeakMap<WebContents, boolean>();
/** Which windows asked for which hosts' live events (`vk:host.events`); reset per document. */
const eventSubs = new EventSubscriptions<WebContents>();
const ttl = (env: string | undefined, fallback: number) => (env && /^\d+$/.test(env) ? Number(env) : fallback);

const windows: Windows = new Windows({
  url: rendererUrl,
  preload: join(outDir, 'preload/index.cjs'),
  stateFile: join(userData, 'window-state.json'),
  beforeShow: (win) => {
    if (stale.delete(win.webContents)) win.webContents.send(EVENT.hosts, engine.fullPatch());
  },
  trayBounds: () => tray.bounds(),
  onNewDocument: (wc) => eventSubs.clear(wc),
  onVisibilityChange: () => {
    // Each window learns its own shown/hidden state (renderers pause display timers while hidden).
    for (const w of windows.all()) {
      const shown = w.isVisible() && !w.isMinimized();
      if (sentVisibility.get(w.webContents) === shown) continue;
      sentVisibility.set(w.webContents, shown);
      w.webContents.send(EVENT.visibility, shown);
    }
    const v = windows.anyVisible();
    if (v === wasVisible) return;
    wasVisible = v;
    for (const f of [...(v ? visible : hidden)]) f();
  },
  devTools: !app.isPackaged || process.env.VIBEKE_DEVTOOLS === '1',
  quickTtlMs: ttl(process.env.VIBEKE_POPOVER_TTL_MS, 60_000),
  mainTtlMs: ttl(process.env.VIBEKE_MAIN_TTL_MS, 10 * 60_000),
});

const iconDir = join(outDir, 'main/assets');
const tray = new AppTray(
  { template: join(iconDir, 'trayTemplate.png'), color: join(iconDir, 'tray.png') },
  {
    toggleQuick: () => windows.toggleQuick(),
    openMain: (hash) => windows.showMain(hash),
    connectLocal: () => windows.showMain('#/pair'),
    quit: () => app.quit(),
    shortcut: () => settings.shortcut,
  },
);

const notifier = new Notifier(
  engine,
  {
    appFocused: () => windows.appFocused(),
    enabled: () => settings.notifications,
    openMain: (hash) => windows.showMain(hash),
    approve: (hash) => windows.showQuick(hash),
    icon: process.platform === 'darwin' ? undefined : join(iconDir, 'icon.png'),
  },
  (o) => (Notification.isSupported() ? new Notification(o) : null),
);

engine.onPatch((patch) => {
  let open = 0;
  for (const s of engine.snapshot()) open += s.dashboard?.interactions.filter((i) => i.status === 'open').length ?? 0;
  tray.setCount(open);
  for (const w of windows.all()) {
    // Hidden windows catch up when shown: no work while hidden beyond the sockets (§16.3).
    if (w.isVisible()) w.webContents.send(EVENT.hosts, patch);
    else stale.add(w.webContents);
  }
});

// Live host events, filtered to the types the UI needs, only to visible windows that subscribed
// for that host (hidden ones catch up from the dashboard when shown).
engine.onEvent((hostId, e) => {
  notifier.onEvent(hostId, e);
  let payload: HostEventPayload | null = null;
  for (const w of windows.all()) {
    if (!w.isVisible() || !eventSubs.wants(w.webContents, hostId)) continue;
    if (!payload) {
      const event = forwardableEvent(e);
      if (!event) return;
      payload = { hostId, event };
    }
    w.webContents.send(EVENT.hostEvent, payload);
  }
});

// ---- settings -------------------------------------------------------------------------------

function applyShortcut(sc: string): void {
  globalShortcut.unregisterAll();
  if (!sc) return;
  if (!globalShortcut.register(sc, () => windows.toggleQuick())) throw Object.assign(new Error(`The shortcut ${sc} is in use by another app.`), { code: 'shortcut_taken' });
}

function applySettings(next: DesktopSettings, prev: DesktopSettings | null): void {
  if (!prev || prev.shortcut !== next.shortcut) applyShortcut(next.shortcut);
  if (!prev || prev.openAtLogin !== next.openAtLogin) {
    // At startup only reconcile a packaged app whose OS state differs; otherwise only when the
    // user flips the switch.
    if (prev) setOpenAtLogin(next.openAtLogin);
    else if (app.isPackaged && next.openAtLogin && process.platform !== 'linux' && !app.getLoginItemSettings().openAtLogin) setOpenAtLogin(true);
  }
  if (process.platform === 'darwin' && (!prev || prev.showDock !== next.showDock)) {
    if (next.showDock) void app.dock?.show();
    else app.dock?.hide();
  }
  Menu.setApplicationMenu(menu());
}

/** Start at login, in the menu bar without a window (`--hidden` / wasOpenedAtLogin). */
function setOpenAtLogin(on: boolean): void {
  if (process.platform === 'linux') {
    // XDG autostart entry (Electron has no login-item API on Linux).
    const dir = join(process.env.XDG_CONFIG_HOME || join(app.getPath('home'), '.config'), 'autostart');
    const file = join(dir, 'vibeke.desktop');
    if (on) {
      mkdirSync(dir, { recursive: true });
      const exec = process.env.APPIMAGE || process.execPath;
      writeFileSync(file, `[Desktop Entry]\nType=Application\nName=Vibeke\nExec="${exec}" --hidden\nX-GNOME-Autostart-enabled=true\n`);
    } else rmSync(file, { force: true });
    return;
  }
  app.setLoginItemSettings({ openAtLogin: on, args: ['--hidden'] });
}

function updateSettings(patch: Partial<DesktopSettings>): DesktopSettings {
  const prev = settings;
  const next = { ...settings, ...patch };
  try {
    applySettings(next, prev);
  } catch (e) {
    // Roll back (e.g. a shortcut another app owns) and report.
    try {
      applySettings(prev, next);
    } catch {
      /* ignore */
    }
    throw e;
  }
  settings = next;
  writeJson(settingsFile, settings);
  for (const w of windows.all()) w.webContents.send(EVENT.settings, settings);
  return settings;
}

const menu = () =>
  buildMenu({
    command: (cmd) => {
      const w = BrowserWindow.getFocusedWindow() ?? windows.main;
      if (!w) return windows.showMain();
      w.webContents.send(EVENT.command, cmd);
    },
    openMain: (hash) => windows.showMain(hash),
    toggleQuick: () => windows.toggleQuick(),
    connectLocal: () => windows.showMain('#/pair'),
    shortcut: () => settings.shortcut,
    devTools: !app.isPackaged,
  });

// ---- local gateway --------------------------------------------------------------------------

/** A path found on PATH outside the usual locations: the picker opens there to confirm it. */
let unexpectedCli: string | null = null;

/** The CLI to run, or why there is none (never silently skips a chosen executable that broke). */
function resolveCli(): { ok: true; bin: string } | Extract<LocalConnectResult, { ok: false }> {
  const r = discoverVibeke(process.env, settings.vibekePath);
  switch (r.kind) {
    case 'found':
      return { ok: true, bin: r.path };
    case 'unexpected':
      unexpectedCli = r.path;
      return { ok: false, code: 'cli_untrusted', message: `Found \`vibeke\` at ${r.path}, outside the usual install locations. Choose it to confirm you trust it.`, detail: r.path };
    case 'invalid':
      return { ok: false, code: 'cli_invalid', message: `The \`vibeke\` at ${r.path} ${r.reason}. Choose a different one.`, detail: r.path };
    case 'missing':
      return { ok: false, code: 'cli_not_found', message: 'The `vibeke` command was not found in ~/.local/bin, /opt/homebrew/bin, /usr/local/bin, ~/.cargo/bin or on PATH.' };
  }
}

async function connectLocal(name: string): Promise<LocalConnectResult> {
  const cli = resolveCli();
  if (!cli.ok) return cli;
  const bin = cli.bin;
  let out;
  try {
    out = await runPairLocal(bin, process.env);
  } catch (e) {
    const code = (e as { code?: string }).code === 'bad_output' ? 'bad_output' : 'cli_failed';
    return { ok: false, code, message: (e as Error).message };
  }
  if (!(await socketAlive(out.socket))) return { ok: false, code: 'gateway_not_running', message: 'The Vibeke gateway is not running on this computer.', detail: out.socket };
  try {
    const record = await engine.pair(out.link, name, () => {});
    return { ok: true, record };
  } catch (e) {
    return { ok: false, code: 'pair_failed', message: (e as Error).message };
  }
}

async function startLocalGateway(): Promise<WireResult<null>> {
  const cli = resolveCli();
  if (!cli.ok) return { ok: false, error: { type: 'error', code: cli.code, message: cli.message } };
  const bin = cli.bin;
  const logFile = join(userData, 'logs', 'gateway.log');
  try {
    startGateway(bin, process.env, logFile);
  } catch (e) {
    return { ok: false, error: { type: 'error', message: (e as Error).message } };
  }
  if (await waitForSocket(gatewaySocket(process.env), 15_000)) return { ok: true, value: null };
  let tail = '';
  try {
    tail = readFileSync(logFile, 'utf8').split('\n').slice(-6).join('\n');
  } catch {
    /* no log */
  }
  const waitingForServer = /waiting for the Vibeke server/i.test(tail);
  return {
    ok: false,
    error: {
      type: 'error',
      code: waitingForServer ? 'server_not_running' : 'gateway_failed',
      message: waitingForServer ? 'The gateway is waiting for the Vibeke server. Start a Vibeke session (or `vibeke server start`), then try again.' : `The gateway did not start. ${tail}`.trim(),
    },
  };
}

/**
 * The only way to set `vibekePath`: a native open panel shown by main, then the same checks as
 * discovery (absolute, canonical, regular executable, owned by this user or root, not writable by
 * others) plus `--version` printing a Vibeke version.
 */
async function chooseVibeke(win: BrowserWindow | null): Promise<ChooseVibekeResult> {
  const opts: Electron.OpenDialogOptions = {
    title: 'Choose the vibeke command',
    buttonLabel: 'Use This vibeke',
    properties: ['openFile', 'showHiddenFiles', 'treatPackageAsDirectory'],
    ...(unexpectedCli ? { defaultPath: unexpectedCli } : {}),
  };
  const r = await (win ? dialog.showOpenDialog(win, opts) : dialog.showOpenDialog(opts));
  const picked = r.canceled ? null : (r.filePaths[0] ?? null);
  if (!picked) return { ok: false, canceled: true };
  const c = checkExecutable(picked);
  if (!c.ok) return { ok: false, canceled: false, message: `${picked} ${c.reason}.` };
  try {
    await vibekeVersion(c.path, process.env);
  } catch (e) {
    return { ok: false, canceled: false, message: (e as Error).message };
  }
  unexpectedCli = null;
  updateSettings({ vibekePath: c.path });
  return { ok: true, path: c.path };
}

/** A native folder panel (new folders allowed); the chosen folder, or null when canceled. */
async function pickDirectory(win: BrowserWindow | null, defaultPath?: string): Promise<string | null> {
  const opts: Electron.OpenDialogOptions = {
    properties: ['openDirectory', 'createDirectory'],
    ...(defaultPath ? { defaultPath } : {}),
  };
  const r = await (win ? dialog.showOpenDialog(win, opts) : dialog.showOpenDialog(opts));
  return r.canceled ? null : (r.filePaths[0] ?? null);
}

// ---- security -------------------------------------------------------------------------------

app.on('web-contents-created', (_e, wc) => {
  // No new windows from page content; http(s) links open in the browser.
  wc.setWindowOpenHandler(({ url }) => {
    try {
      void shell.openExternal(externalUrl(url));
    } catch {
      log('blocked window.open');
    }
    return { action: 'deny' };
  });
  const guard = (e: Electron.Event, url: string) => {
    if (!isTrustedUrl(url, trustedOrigins())) {
      e.preventDefault();
      log('blocked navigation');
    }
  };
  wc.on('will-navigate', guard);
  wc.on('will-redirect', guard);
  wc.on('will-attach-webview', (e) => e.preventDefault());
});

function secureSession(): void {
  const s = session.defaultSession;
  s.setPermissionRequestHandler((wc, permission, cb, details) => {
    const trusted = isTrustedUrl(details.requestingUrl, trustedOrigins());
    // Microphone for voice input (host-side transcription); clipboard writes from copy buttons.
    if (trusted && permission === 'media') return cb((details as { mediaTypes?: string[] }).mediaTypes?.every((t) => t === 'audio') ?? false);
    cb(trusted && permission === 'clipboard-sanitized-write');
  });
  s.setPermissionCheckHandler((_wc, permission, origin) => isTrustedUrl(origin, trustedOrigins()) && (permission === 'clipboard-sanitized-write' || permission === 'media'));
  if (devServer) {
    // The bundled app gets its CSP from the app:// handler; the dev server gets a looser one.
    s.webRequest.onHeadersReceived((d, cb) => {
      const h = { ...d.responseHeaders };
      if (d.url.startsWith(devServer)) h['Content-Security-Policy'] = [CSP.replace("script-src 'self'", "script-src 'self' 'unsafe-inline'").replace("connect-src 'self'", "connect-src 'self' ws://localhost:*")];
      cb({ responseHeaders: h });
    });
  }
}

// ---- ready ----------------------------------------------------------------------------------

function accent(): string | null {
  try {
    if (process.platform !== 'darwin' && process.platform !== 'win32') return null;
    const c = systemPreferences.getAccentColor();
    return /^[0-9a-f]{6,8}$/i.test(c) ? `#${c.slice(0, 6)}` : null;
  } catch {
    return null;
  }
}

app.whenReady().then(() => {
  if (!devServer) handleAppProtocol(join(outDir, 'renderer'));
  secureSession();

  registerIpc({
    engine,
    setHostEvents: (wc, hostId, on) => eventSubs.set(wc, hostId, on),
    windows,
    trusted: trustedOrigins,
    settings: () => settings,
    updateSettings,
    connectLocal,
    startLocalGateway,
    chooseVibeke,
    resetVibeke: () => updateSettings({ vibekePath: '' }),
    pickDirectory,
    onPrefsChanged: (h) => notifier.invalidatePrefs(h),
    onTheme: () => windows.repaint(),
    log,
  });
  // Boot facts (registered here because it needs `settings`); same sender checks as ipc.ts.
  ipcMain.handle(INVOKE.boot, (e) => {
    if (!isTrustedUrl(e.senderFrame?.url, trustedOrigins()) || !windows.isOurs(e.sender)) throw new Error('forbidden');
    const info: BootInfo = {
      platform: process.platform,
      platformName,
      deviceName: deviceName(),
      version,
      accent: accent(),
      settings,
      localGateway: existsSync(gatewaySocket(process.env)),
    };
    return info;
  });

  tray.create();
  try {
    applySettings(settings, null);
  } catch (e) {
    log((e as Error).message);
    Menu.setApplicationMenu(menu());
  }
  nativeTheme.on('updated', () => windows.repaint());
  powerMonitor.on('resume', () => engine.reconnect(null));
  powerMonitor.on('unlock-screen', () => engine.reconnect(null));

  engine.start().then(
    () => notifier.start(),
    (e) => log(`engine: ${(e as Error).message}`),
  );
  startUpdates(log);

  const hiddenStart = process.argv.includes('--hidden') || (process.platform === 'darwin' && app.getLoginItemSettings().wasOpenedAtLogin);
  ready = true;
  const link = pendingLink ? deepLinkToHash(pendingLink) : null;
  pendingLink = null;
  if (link) windows.showMain(link);
  else if (!hiddenStart) windows.showMain();
  log(`ready ${version} (${typeof __BUILD_HASH__ === 'string' ? __BUILD_HASH__ : 'dev'})`);
});

app.on('activate', () => windows.showMain());
app.on('before-quit', () => {
  windows.quitting = true;
});
app.on('will-quit', () => {
  globalShortcut.unregisterAll();
  notifier.stop();
  engine.stop();
  tray.destroy();
});
// Stay alive in the menu bar / tray with no windows open.
app.on('window-all-closed', () => {});
