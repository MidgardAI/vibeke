// Renderer containment (spec 16 §16.1, review item 13): the bundled page is served with a strict
// CSP that is actually enforced; page content can neither open windows nor navigate away; the
// bridge refuses unknown channels; main refuses non-allow-listed host methods and forged
// arguments. Runs against the built app without a CLI or hosts.

import { rmSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import { built, hasDisplay, launchApp, shortTmp, type LaunchedApp } from './helpers';

type Bridge = { vibeke: { invoke(c: string, ...a: unknown[]): Promise<{ ok: boolean; error?: { message: string } }> } };

test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'run `bun run build` first');

let a: LaunchedApp;
let gw: string;

test.beforeAll(async () => {
  gw = shortTmp('vkgw-');
  a = await launchApp({ VIBEKE_GATEWAY_DIR: gw, VIBEKE_BIN: '/nonexistent/vibeke', PATH: '/usr/bin:/bin' });
  await expect(a.page.getByText('Pair with a host')).toBeVisible();
  // Record instead of launching the user's browser.
  await a.app.evaluate(({ shell }) => {
    const g = globalThis as unknown as { __opened: string[] };
    g.__opened = [];
    shell.openExternal = async (u: string) => void g.__opened.push(u);
  });
});

test.afterAll(async () => {
  await a?.close();
  if (gw) rmSync(gw, { recursive: true, force: true });
});

const opened = () => a.app.evaluate(() => (globalThis as unknown as { __opened: string[] }).__opened.slice());
const windowCount = () => a.app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows().length);

test('the page is served with a strict CSP, and it is enforced', async () => {
  const { page } = a;
  expect(page.url()).toMatch(/^app:\/\/vibeke\//);
  const csp = await page.evaluate(async () => (await fetch(location.href)).headers.get('content-security-policy'));
  expect(csp).toBeTruthy();
  const directives = Object.fromEntries(csp!.split(';').map((d) => d.trim().split(/\s+/)).map(([k, ...v]) => [k, v.join(' ')]));
  expect(directives['default-src']).toBe("'self'");
  expect(directives['script-src']).toBe("'self'");
  expect(directives['connect-src']).toBe("'self'");
  expect(directives['object-src']).toBe("'none'");
  expect(directives['frame-src']).toBe("'none'");
  expect(directives['base-uri']).toBe("'none'");
  expect(directives['form-action']).toBe("'none'");
  // Enforced, not just sent: no inline script, no eval, no network from the page.
  const enforced = await page.evaluate(async () => {
    const violations: string[] = [];
    document.addEventListener('securitypolicyviolation', (e) => violations.push(e.effectiveDirective));
    const w = window as unknown as { __inline?: boolean };
    const s = document.createElement('script');
    s.textContent = 'window.__inline = true';
    document.head.appendChild(s);
    // From a page task, not inside this evaluation: DevTools' Runtime.evaluate is exempt from
    // the eval restriction (allowUnsafeEvalBlockedByCSP), page code is not.
    const evalBlocked = await new Promise<boolean>((r) =>
      setTimeout(() => {
        try {
          // eslint-disable-next-line no-eval
          (0, eval)('1');
          r(false);
        } catch {
          r(true);
        }
      }, 0),
    );
    const fetchBlocked = await fetch('https://example.com/').then(() => false, () => true);
    await new Promise((r) => setTimeout(r, 200));
    return { inline: !!w.__inline, evalBlocked, fetchBlocked, violations };
  });
  expect(enforced.inline).toBe(false);
  expect(enforced.evalBlocked).toBe(true);
  expect(enforced.fetchBlocked).toBe(true);
  expect(enforced.violations).toEqual(expect.arrayContaining(['script-src-elem', 'script-src', 'connect-src']));
});

test('window.open never opens an app window', async () => {
  const { page } = a;
  const before = await windowCount();
  const results = await page.evaluate(() => [window.open('https://example.com/'), window.open('file:///etc/passwd'), window.open('app://vibeke/index.html')].map((w) => w === null));
  expect(results).toEqual([true, true, true]);
  await page.waitForTimeout(500);
  expect(await windowCount()).toBe(before);
  // Only the http(s) one is handed to the system browser; nothing else leaves the app.
  expect(await opened()).toEqual(['https://example.com/']);
});

test('the bridge refuses unknown channels; main refuses other methods and forged arguments', async () => {
  const { page } = a;
  const r = await page.evaluate(async () => {
    const b = (window as unknown as Bridge).vibeke;
    const outcome = (p: Promise<{ ok: boolean }>) =>
      p.then(
        (x) => (x.ok ? 'ok' : 'refused'),
        (e: Error) => `rejected: ${e.message}`,
      );
    return {
      unknownChannel: await outcome(b.invoke('vk:evil')),
      rawElectronChannel: await outcome(b.invoke('ELECTRON_BROWSER_REQUIRE', 'child_process')),
      eventAsInvoke: await outcome(b.invoke('vk:hosts')),
      notAllowListedMethod: await outcome(b.invoke('vk:host.request', 'h1', 'server.shutdown', {})),
      forgedHost: await outcome(b.invoke('vk:host.request', '../../etc', 'dashboard.get', {})),
      forgedParams: await outcome(b.invoke('vk:host.request', 'h1', 'dashboard.get', new Date())),
      forgedOpts: await outcome(b.invoke('vk:host.request', 'h1', 'dashboard.get', {}, { timeoutMs: 1e9, mutating: false })),
      forgedUrl: await outcome(b.invoke('vk:shell.open-external', 'file:///etc/passwd')),
      forgedWindow: await outcome(b.invoke('vk:window', { op: 'pop-out', host: 'h1', pane: 'p\u0000' })),
      unknownWindowOp: await outcome(b.invoke('vk:window', { op: 'devtools' })),
      forgedTheme: await outcome(b.invoke('vk:theme', 'hacker')),
      forgedSettings: await outcome(b.invoke('vk:settings.set', { vibekePath: '/tmp/evil' })),
      notAFunction: typeof (window as unknown as { vibeke: { ipcRenderer?: unknown } }).vibeke.ipcRenderer,
    };
  });
  expect(r.unknownChannel).toBe('rejected: unknown channel vk:evil');
  expect(r.rawElectronChannel).toMatch(/^rejected: unknown channel/);
  expect(r.eventAsInvoke).toMatch(/^rejected: unknown channel/);
  for (const k of ['notAllowListedMethod', 'forgedHost', 'forgedParams', 'forgedOpts', 'forgedUrl', 'forgedWindow', 'unknownWindowOp', 'forgedTheme'] as const) {
    expect(r[k], k).toMatch(/^rejected: .*invalid IPC/);
  }
  expect(r.forgedSettings).not.toBe('ok');
  expect(r.notAFunction).toBe('undefined');
  expect(await opened()).not.toContain('file:///etc/passwd');
});

// Last: Playwright's page waits forever on a renderer navigation that main cancelled, so this
// test drives and inspects the page from the main process only.
test('page content cannot navigate the window away', async () => {
  const main = () => a.app.evaluate(({ BrowserWindow }) => {
    const w = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL().includes('surface=full'))!;
    return w.webContents.getURL();
  });
  const url = await main();
  expect(url).toMatch(/^app:\/\/vibeke\//);
  for (const target of ['https://example.com/', 'file:///etc/passwd', 'http://127.0.0.1:1/', 'app://evil/index.html']) {
    await a.app.evaluate(async ({ BrowserWindow }, t) => {
      const w = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL().includes('surface=full'))!;
      await w.webContents.executeJavaScript(`location.href = ${JSON.stringify(t)}; void 0`);
    }, target);
    await new Promise((r) => setTimeout(r, 500));
    expect(await main(), target).toBe(url);
  }
  // Still the live app (not a blank or error page).
  const alive = await a.app.evaluate(({ BrowserWindow }) => {
    const w = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL().includes('surface=full'))!;
    return w.webContents.executeJavaScript(`document.body.innerText.includes('Pair with a host')`);
  });
  expect(alive).toBe(true);
});
