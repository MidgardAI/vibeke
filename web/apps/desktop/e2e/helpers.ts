// Shared e2e helpers: launch the built app in an isolated profile, and an isolated real Vibeke
// server + gateway (temp HOME / XDG / runtime dirs, so nothing touches the user's own session).

import { execFileSync, spawn, type ChildProcess } from 'node:child_process';
import { existsSync, mkdirSync, mkdtempSync, readdirSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { _electron as electron, type ElectronApplication, type Page } from '@playwright/test';

export const appRoot = new URL('..', import.meta.url).pathname;
const repoRoot = new URL('../../../../', import.meta.url).pathname;

/** No GUI session → skip (Linux CI without Xvfb; macOS always has a window server here). */
export function hasDisplay(): boolean {
  if (process.platform === 'linux') return !!(process.env.DISPLAY || process.env.WAYLAND_DISPLAY);
  return true;
}

export function built(): boolean {
  return existsSync(join(appRoot, 'out/main/index.cjs')) && existsSync(join(appRoot, 'out/renderer/index.html'));
}

/** A short scratch root: Unix socket paths are limited to ~104 bytes on macOS. */
export function shortTmp(prefix: string): string {
  const base = process.env.VIBEKE_E2E_TMP ?? (existsSync('/private/tmp/claude-501') ? '/private/tmp/claude-501' : '/tmp');
  mkdirSync(base, { recursive: true });
  return mkdtempSync(join(base, prefix));
}

export interface LaunchedApp {
  app: ElectronApplication;
  page: Page;
  userData: string;
  close(): Promise<void>;
}

export async function launchApp(env: Record<string, string> = {}, extraArgs: string[] = []): Promise<LaunchedApp> {
  const userData = mkdtempSync(join(tmpdir(), 'vibeke-desktop-e2e-'));
  const app = await electron.launch({
    // `--use-mock-keychain`: Chromium's test keychain, so safeStorage never prompts.
    args: [appRoot, '--use-mock-keychain', ...extraArgs],
    cwd: appRoot,
    env: { ...(process.env as Record<string, string>), VIBEKE_USER_DATA: userData, ELECTRON_ENABLE_LOGGING: '1', ...env },
    timeout: 30_000,
  });
  const page = await mainWindow(app);
  return {
    app,
    page,
    userData,
    async close() {
      await app.close().catch(() => {});
      rmSync(userData, { recursive: true, force: true });
    },
  };
}

/** The main window (the quick popover and pane windows load `?surface=…`). */
export async function mainWindow(app: ElectronApplication): Promise<Page> {
  for (let i = 0; i < 100; i++) {
    const w = app.windows().find((p) => /surface=full/.test(p.url()));
    if (w) {
      await w.waitForLoadState('domcontentloaded');
      return w;
    }
    await new Promise((r) => setTimeout(r, 100));
  }
  throw new Error('main window did not open');
}

export function vibekeBin(): string | null {
  // CI passes VIBEKE_BIN; VIBEKE_TEST_BIN is the older name; else the workspace's debug build.
  const p = process.env.VIBEKE_BIN || process.env.VIBEKE_TEST_BIN || join(repoRoot, 'target/debug/vibeke');
  return existsSync(p) ? p : null;
}

/** A real server + gateway in temp dirs. `stop()` stops both and any pane holders. */
export class TestHost {
  readonly root: string;
  readonly env: Record<string, string>;
  private procs: ChildProcess[] = [];
  readonly session = 't';

  constructor(readonly bin: string) {
    this.root = shortTmp('vkd-');
    const d = (n: string) => {
      const p = join(this.root, n);
      mkdirSync(p, { recursive: true });
      return p;
    };
    const home = d('home');
    this.env = {
      PATH: `${join(bin, '..')}:/usr/bin:/bin:/usr/sbin:/sbin`,
      HOME: home,
      USER: process.env.USER ?? 'test',
      SHELL: '/bin/sh',
      TERM: 'xterm-256color',
      LANG: 'en_US.UTF-8',
      VIBEKE_RUNTIME_DIR: d('rt'),
      VIBEKE_GATEWAY_DIR: d('gw'),
      XDG_STATE_HOME: d('state'),
      XDG_CONFIG_HOME: d('cfg'),
      XDG_DATA_HOME: d('data'),
      CLAUDE_CONFIG_DIR: join(home, '.claude'),
      CODEX_HOME: join(home, '.codex'),
      TMPDIR: d('tmp'),
    };
  }

  get gatewayDir(): string {
    return this.env.VIBEKE_GATEWAY_DIR!;
  }

  cli(args: string[], input?: string): string {
    return execFileSync(this.bin, ['--session', this.session, ...args], { env: this.env, encoding: 'utf8', input, timeout: 20_000 });
  }

  /** A CLI command without `--session` (e.g. `gateway …`, which takes no session). */
  run(args: string[]): string {
    return execFileSync(this.bin, args, { env: this.env, encoding: 'utf8', timeout: 20_000 });
  }

  private spawn(args: string[], log: string): ChildProcess {
    const p = spawn(this.bin, args, { env: this.env, stdio: ['ignore', 'pipe', 'pipe'] });
    const chunks: string[] = [];
    p.stdout?.on('data', (b) => chunks.push(String(b)));
    p.stderr?.on('data', (b) => chunks.push(String(b)));
    p.on('exit', () => (this.logs[log] = chunks.join('')));
    this.logs[log] = '';
    Object.defineProperty(this.logs, `${log}:live`, { get: () => chunks.join(''), configurable: true });
    this.procs.push(p);
    return p;
  }
  logs: Record<string, string> = {};

  async start(): Promise<void> {
    this.spawn(['--session', this.session, 'server', 'start'], 'server');
    await this.until(() => {
      try {
        return JSON.parse(this.cli(['server', 'status'])).pid > 0;
      } catch {
        return false;
      }
    }, 20_000, 'server did not start');
    this.spawn(['gateway', 'run', '--session', this.session], 'gateway');
    await this.until(() => existsSync(join(this.gatewayDir, 'gateway.sock')), 20_000, 'gateway local socket did not appear');
  }

  async until(f: () => boolean, ms: number, what: string): Promise<void> {
    const end = Date.now() + ms;
    while (Date.now() < end) {
      if (f()) return;
      await new Promise((r) => setTimeout(r, 200));
    }
    throw new Error(what);
  }

  /** A workspace with one shell pane; returns the pane handle. */
  workspace(name: string): { pane: string; cwd: string } {
    const cwd = join(this.root, name);
    mkdirSync(cwd, { recursive: true });
    const r = JSON.parse(this.cli(['workspace', 'create', '--cwd', cwd, '--name', name]));
    return { pane: r.root_pane.handle as string, cwd };
  }

  /**
   * Make the shell in `pane` ask for a Claude-style permission (`vibeke hook claude
   * PermissionRequest`): an open, low-risk approval interaction that blocks until answered.
   */
  requestApproval(pane: string, command: string, cwd: string): void {
    const payload = JSON.stringify({ session_id: 'e2e-session', hook_event_name: 'PermissionRequest', tool_name: 'Bash', tool_input: { command }, cwd });
    this.cli(['pane', 'send-text', pane, `echo '${payload}' | '${this.bin}' hook claude PermissionRequest; echo HOOK-DONE`]);
    this.cli(['pane', 'send-keys', pane, 'enter']);
  }

  interactions(status = 'all'): { id: string; status: string; title: string; answer: { decision: string | null } | null }[] {
    return JSON.parse(this.cli(['interaction', 'list', '--status', status])).interactions;
  }

  async stop(): Promise<void> {
    try {
      this.cli(['server', 'stop']);
    } catch {
      // already down
    }
    for (const p of this.procs) if (p.exitCode === null) p.kill('SIGTERM');
    await new Promise((r) => setTimeout(r, 300));
    for (const p of this.procs) if (p.exitCode === null) p.kill('SIGKILL');
    // Pane holders (one process per pane) live under the runtime dir.
    killUnder(this.env.VIBEKE_RUNTIME_DIR!);
    killUnder(this.root);
    rmSync(this.root, { recursive: true, force: true });
  }
}

/** Kill processes whose command line mentions `dir` (pane holders, shells started there). */
function killUnder(dir: string): void {
  try {
    const out = execFileSync('ps', ['-axo', 'pid=,command='], { encoding: 'utf8' });
    for (const line of out.split('\n')) {
      const m = /^\s*(\d+)\s+(.*)$/.exec(line);
      if (m && m[2]!.includes(dir) && Number(m[1]) !== process.pid) {
        try {
          process.kill(Number(m[1]), 'SIGKILL');
        } catch {
          /* gone */
        }
      }
    }
  } catch {
    /* ps unavailable */
  }
}

export const readLog = (p: string): string => (existsSync(p) ? readFileSync(p, 'utf8') : '');
export const listDir = (p: string): string[] => (existsSync(p) ? readdirSync(p) : []);

/** Wait until every finite animation/transition on the page has finished (screenshots). */
export async function settled(page: Page): Promise<void> {
  await page.evaluate(async () => {
    for (let i = 0; i < 3; i++) {
      const running = document.getAnimations().filter((a) => {
        const it = a.effect?.getTiming().iterations;
        return a.playState === 'running' && it !== Infinity;
      });
      if (!running.length) break;
      await Promise.all(running.map((a) => a.finished.catch(() => {})));
    }
    await new Promise((r) => requestAnimationFrame(() => requestAnimationFrame(r)));
  });
}

/** Switch the native appearance (vibrancy material, title bar) to light/dark. */
export async function appearance(app: ElectronApplication, mode: 'light' | 'dark'): Promise<void> {
  await app.evaluate(({ nativeTheme }, m) => {
    nativeTheme.themeSource = m;
  }, mode);
  await new Promise((r) => setTimeout(r, 250));
}

const VIBRANCY_STANDIN = `
  :root[data-vibrancy][data-theme="light"] body { background: #e9e9eb; }
  :root[data-vibrancy][data-theme="dark"] body { background: #232326; }
`;

/**
 * macOS: capture the real window (native vibrancy, title bar, shadow) with `screencapture -l`.
 * Needs the Screen Recording permission for the terminal; returns false when unavailable.
 */
async function nativeShot(app: ElectronApplication, page: Page, path: string): Promise<boolean> {
  if (process.platform !== 'darwin' || process.env.VIBEKE_E2E_NATIVE_SHOTS === '0') return false;
  const id = await app.evaluate(({ BrowserWindow }, url) => {
    const w = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL() === url);
    return w ? Number(w.getMediaSourceId().split(':')[1]) : null;
  }, page.url());
  if (!id) return false;
  try {
    execFileSync('screencapture', ['-x', '-o', `-l${id}`, path], { timeout: 10_000, stdio: 'ignore' });
    return existsSync(path);
  } catch {
    return false;
  }
}

/**
 * Screenshot in light and dark, after animations settle: `<name>-light.png`, `<name>-dark.png`.
 * The app's own theme preference is overridden for the capture and restored afterwards.
 */
export async function shoot(app: ElectronApplication, page: Page, name: string): Promise<void> {
  const prev = await page.evaluate(() => document.documentElement.getAttribute('data-theme'));
  for (const mode of ['light', 'dark'] as const) {
    await appearance(app, mode);
    // Playwright pins prefers-color-scheme to light unless told otherwise.
    await page.emulateMedia({ colorScheme: mode });
    await page.evaluate((m) => document.documentElement.setAttribute('data-theme', m), mode);
    await settled(page);
    const path = join(appRoot, 'test-results', `${name}-${mode}.png`);
    if (!(await nativeShot(app, page, path))) {
      // A page screenshot cannot see native vibrancy (the sidebar/popover are transparent there):
      // paint an approximation of the macOS sidebar material behind them for the capture only.
      await page.addStyleTag({ content: VIBRANCY_STANDIN });
      await page.screenshot({ path });
    }
  }
  await page.evaluate((t) => (t ? document.documentElement.setAttribute('data-theme', t) : document.documentElement.removeAttribute('data-theme')), prev);
  await appearance(app, 'light');
  await page.emulateMedia({ colorScheme: 'light' });
}

/** Make `pane`'s shell run a Claude-style hook event (session start, prompt, stop…). */
export function hookEvent(host: TestHost, pane: string, event: string, cwd: string, extra: Record<string, unknown> = {}): void {
  const payload = JSON.stringify({ session_id: `e2e-${pane}`, hook_event_name: event, cwd, ...extra }).replace(/'/g, '');
  host.cli(['pane', 'send-text', pane, `echo '${payload}' | '${host.bin}' hook claude ${event}`]);
  host.cli(['pane', 'send-keys', pane, 'enter']);
}
