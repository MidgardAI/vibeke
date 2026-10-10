// Real browser + Rust/WASM TUI + native server + encrypted local relay.
// Run from web/: bun apps/site/scripts/check-wasm-tui.ts
import { chromium, firefox, webkit, expect } from '@playwright/test';
import { spawn, execFileSync, type ChildProcess } from 'node:child_process';
import { mkdtempSync, rmSync, readFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { createServer as createHttpServer, type Server } from 'node:http';
import { readFile } from 'node:fs/promises';
import { resolve, extname } from 'node:path';
import { fileURLToPath } from 'node:url';
import sharp from 'sharp';

const root = resolve(fileURLToPath(new URL('../../../../', import.meta.url)));
const binary = process.env.VIBEKE_TEST_BINARY ?? resolve(root, 'target/debug/vibeke');
const temp = mkdtempSync('/tmp/vkw-');
const env = { ...process.env, VIBEKE_RUNTIME_DIR: `${temp}/run`, VIBEKE_STATE_DIR: `${temp}/state`, VIBEKE_CONFIG: `${temp}/config.toml`, VIBEKE_GATEWAY_DIR: `${temp}/gateway` };
for (const name of ['VIBEKE', 'VIBEKE_SOCKET', 'VIBEKE_SESSION', 'VIBEKE_PANE_TOKEN', 'NO_COLOR']) delete env[name as keyof typeof env];
const children: ChildProcess[] = [];
let staticServer: Server | undefined;
const logs: string[] = [];
const start = (exe: string, args: string[], extra = {}) => {
  const child = spawn(exe, args, { cwd: root, env: { ...env, ...extra }, stdio: ['ignore', 'pipe', 'pipe'] });
  let output = '';
  child.stdout!.on('data', (b) => { output += b; });
  child.stderr!.on('data', (b) => { logs.push(String(b)); });
  children.push(child);
  return { child, output: () => output };
};
const cli = (...args: string[]) => execFileSync(binary, args[0] === 'gateway' ? args : ['--json', ...args], { cwd: temp, env, encoding: 'utf8', timeout: 30_000 });
async function port() {
  const server = createServer();
  await new Promise<void>((done) => server.listen(0, '127.0.0.1', done));
  const n = (server.address() as { port: number }).port;
  await new Promise<void>((done) => server.close(() => done()));
  return n;
}
const engine = process.env.VIBEKE_TUI_BROWSER ?? 'chromium';
const browser = await ({ chromium, firefox, webkit }[engine] ?? chromium).launch({ headless: true, executablePath: process.env.VIBEKE_TUI_BROWSER_EXECUTABLE });
console.log('Browser engine:', engine, browser.version());
try {
  const relay = `http://127.0.0.1:${await port()}`;
  const origin = `http://127.0.0.1:${await port()}`;
  start(binary, ['relay', '--listen', relay.replace('http://', ''), '--public-url', relay]);
  if (process.env.VIBEKE_TUI_PRODUCTION) {
    const output = `${root}/web/apps/pwa/.vercel/output`;
    const config = JSON.parse(readFileSync(`${output}/config.json`, 'utf8'));
    const headers = { ...config.routes[0].headers };
    // The packaged policy requires secure relays. Only this isolated HTTP test adds its
    // exact loopback relay; script/WASM execution uses the packaged policy unchanged.
    headers['Content-Security-Policy'] = headers['Content-Security-Policy'].replace("connect-src 'self' wss:", `connect-src 'self' wss: ${relay.replace('http:', 'ws:')}`);
    staticServer = createHttpServer(async (request, response) => {
      const pathname = new URL(request.url ?? '/', origin).pathname;
      const path = resolve(output, 'static', '.' + decodeURIComponent(pathname === '/' ? '/index.html' : pathname));
      if (!path.startsWith(`${output}/static/`)) { response.writeHead(404).end(); return; }
      try {
        const file = await readFile(path);
        const mime: Record<string, string> = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm', '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png', '.woff2': 'font/woff2', '.webmanifest': 'application/manifest+json' };
        response.writeHead(200, { ...headers, 'Content-Type': mime[extname(path)] ?? 'application/octet-stream' }).end(file);
      } catch { response.writeHead(404).end(); }
    });
    await new Promise<void>((done) => staticServer!.listen(Number(new URL(origin).port), '127.0.0.1', done));
  } else {
    start(process.env.VIBEKE_BUN ?? 'bun', ['run', '--cwd', `${root}/web/apps/pwa`, 'dev', '--host', '127.0.0.1', '--port', new URL(origin).port, '--strictPort'], { VIBEKE_WASM_TUI: '1' });
  }
  await expect.poll(async () => { try { return (await fetch(origin)).ok; } catch { return false; } }, { timeout: 30_000 }).toBe(true);
  console.log('App and relay started');
  writeFileSync(env.VIBEKE_CONFIG, '[terminal]\ndefault_shell = "/bin/sh"\nshell_mode = "non_login"\n', { mode: 0o600 });
  const ws = JSON.parse(cli('workspace', 'create', '--cwd', temp, '--command', '/bin/sh', '--name', 'WASM experiment'));
  const pane = ws.root_pane.id as string;
  mkdirSync(`${temp}/gateway`, { recursive: true, mode: 0o700 });
  writeFileSync(`${temp}/gateway/gateway.toml`, 'host_name = "WASM host"\n', { mode: 0o600 });
  const pair = start(binary, ['gateway', 'pair', '--relay', relay, '--app-url', origin, '--no-confirm', '--no-qr']);
  await expect.poll(() => pair.output().match(/http:\/\/[^\s]+\/#\/pair\?d=[^\s]+/)?.[0], { timeout: 30_000 }).toBeTruthy();
  const link = pair.output().match(/http:\/\/[^\s]+\/#\/pair\?d=[^\s]+/)![0];
  const context = await browser.newContext({ viewport: { width: 1280, height: 850 }, locale: 'en-US', deviceScaleFactor: Number(process.env.VIBEKE_TUI_DPR ?? 1) });
  context.setDefaultTimeout(15_000);
  const page = await context.newPage();
  page.on('crash', () => logs.push('Browser page crashed'));
  const errors: string[] = [];
  page.on('pageerror', (e) => { errors.push(e.stack ?? String(e)); logs.push(e.stack ?? String(e)); });
  page.on('console', (m) => { if (m.type() === 'error') logs.push(m.text()); });
  await page.goto(link);
  await page.getByRole('button', { name: 'Pair', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Open Vibeke', exact: true })).toBeVisible({ timeout: 30_000 });
  console.log('Browser paired');
  await page.bringToFront();
  await page.getByRole('button', { name: 'Open terminal', exact: true }).focus();
  await page.keyboard.press('Enter');
  await expect(page.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  console.log('TUI connected');
  const menu = async () => page.getByRole('button', { name: 'Browser menu', exact: true }).click();
  const closeMenu = async () => page.getByRole('button', { name: 'Close', exact: true }).click();
  const terminal = page.locator('.xterm-accessibility-tree');
  const screenContains = async (text: string, timeout = 20_000) => {
    await menu();
    await page.getByRole('checkbox', { name: 'Screen reader support', exact: true }).check();
    await closeMenu();
    await expect(terminal).toContainText(text, { timeout });
    await menu();
    await page.getByRole('checkbox', { name: 'Screen reader support', exact: true }).uncheck();
    await closeMenu();
    await page.locator('.xterm-helper-textarea').focus();
  };
  await screenContains('WASM experiment', 20_000);
  await page.locator('.xterm-helper-textarea').focus();
  await page.keyboard.press('Control+Shift+.');
  await expect(page.getByRole('dialog', { name: 'Browser terminal controls' })).toBeVisible();
  await page.keyboard.press('Shift+Tab');
  await expect(page.getByRole('button', { name: 'Leave terminal', exact: true })).toBeFocused();
  await page.keyboard.press('Escape');
  await expect(page.locator('.xterm-helper-textarea')).toBeFocused();
  await page.keyboard.type("printf 'WASM_REMOTE_%s\\n' 'OK'\n");
  await screenContains('WASM_REMOTE_OK', 20_000);
  // Browser text insertion must preserve multi-byte characters.
  await page.keyboard.insertText("printf 'Unicode: café 日本語\\n'");
  await page.keyboard.press('Enter');
  await screenContains('Unicode: café 日本語', 20_000);
  expect(JSON.parse(cli('pane', 'read', pane, '--source', 'screen', '--lines', '100')).text).toContain('WASM_REMOTE_OK');
  await page.keyboard.type("printf '\\033[48;2;18;171;205m TRUECOLOR \\033[0m\\n'\n");
  await screenContains('TRUECOLOR');
  await expect.poll(async () => {
    const pixels = await sharp(await page.screenshot()).removeAlpha().raw().toBuffer();
    let matches = 0;
    for (let i = 0; i < pixels.length; i += 3) if (pixels[i] === 18 && pixels[i + 1] === 171 && pixels[i + 2] === 205) matches++;
    return matches;
  }).toBeGreaterThan(50);
  // Layout changes must not recreate the client or erase its screen.
  await menu();
  await page.getByRole('spinbutton', { name: 'Terminal font size' }).fill('16');
  await page.getByRole('combobox', { name: 'Terminal theme' }).selectOption('light');
  await closeMenu();
  await screenContains('WASM_REMOTE_OK');
  await menu();
  await page.getByRole('combobox', { name: 'Terminal theme' }).selectOption('dark');
  await closeMenu();
  // A second browser client has its own render attachment and can release it independently.
  const second = await context.newPage();
  await second.goto(page.url());
  await expect(second.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  await second.setViewportSize({ width: 900, height: 600 });
  await second.close();
  await page.bringToFront();
  await screenContains('WASM_REMOTE_OK');
  // A terminal mounted behind the app lock must initialize its Rust visibility
  // even though user input is gated. Observe the public WASM method in this test page.
  const lockedPage = await context.newPage();
  await lockedPage.goto(`${origin}/#/settings/system`);
  await lockedPage.getByRole('button', { name: 'Lock now', exact: true }).click();
  await expect(lockedPage.getByRole('alertdialog')).toBeVisible();
  const visibilityManifest = await (await fetch(`${origin}/tui/manifest.json`)).json() as { moduleUrl: string };
  await lockedPage.evaluate(async (url) => {
    const module = await import(url);
    const original = module.BrowserTui.prototype.visible;
    const observed = window as unknown as { tuiVisibility: boolean[] };
    observed.tuiVisibility = [];
    module.BrowserTui.prototype.visible = function (visible: boolean) { observed.tuiVisibility.push(visible); return original.call(this, visible); };
  }, visibilityManifest.moduleUrl);
  await lockedPage.goto(page.url());
  await expect.poll(() => lockedPage.evaluate(() => (window as unknown as { tuiVisibility: boolean[] }).tuiVisibility)).toContain(false);
  await lockedPage.getByRole('button', { name: 'Resume', exact: true }).click();
  await expect(lockedPage.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  await lockedPage.close();
  await page.bringToFront();
  // Exercise the visibility transition deterministically, even in headless engines that
  // keep all pages visible. Protocol work continues while no animation frame is painted.
  await page.evaluate(() => { Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'hidden' }); document.dispatchEvent(new Event('visibilitychange')); });
  cli('pane', 'send-text', pane, "printf 'BACKGROUND_%s\\n' 'OK'");
  cli('pane', 'send-keys', pane, 'enter');
  await expect.poll(() => JSON.parse(cli('pane', 'read', pane, '--source', 'screen', '--lines', '100')).text).toContain('BACKGROUND_OK');
  await page.evaluate(() => { Object.defineProperty(document, 'visibilityState', { configurable: true, value: 'visible' }); document.dispatchEvent(new Event('visibilitychange')); });
  await screenContains('BACKGROUND_OK');
  // Denied clipboard permissions must leave a usable manual path.
  await page.evaluate(() => { Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
    readText: async () => { throw new Error('denied'); }, writeText: async () => { throw new Error('denied'); },
  } }); });
  await menu();
  await page.getByRole('button', { name: 'Copy terminal link', exact: true }).click();
  await expect(page.getByLabel('Text to copy')).toHaveValue(/#\/tui\/.*\?workspace=.*&pane=/);
  const bookmark = await page.getByLabel('Text to copy').inputValue();
  await page.getByRole('button', { name: 'Paste', exact: true }).click();
  await page.getByLabel('Paste here with your browser shortcut').fill("printf 'PASTE_%s\\n' 'ONE'\nprintf 'PASTE_%s\\n' 'TWO'\n");
  await page.getByRole('button', { name: 'Send paste', exact: true }).click();
  await screenContains('PASTE_TWO');
  const bookmarked = await context.newPage();
  await bookmarked.goto(bookmark);
  await expect(bookmarked.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  await bookmarked.close();
  await page.bringToFront();
  await menu();
  await page.getByRole('button', { name: 'Commands', exact: true }).click();
  await screenContains('command palette', 5000);
  await page.locator('.xterm-helper-textarea').focus();
  await page.keyboard.press('Escape');
  await page.keyboard.press('Control+b');
  await page.keyboard.press('v');
  await expect.poll(() => JSON.parse(cli('pane', 'list')).panes.length, { timeout: 15_000 }).toBe(2);
  await page.setViewportSize({ width: 1000, height: 700 });
  await menu();
  await page.getByRole('button', { name: 'Reconnect', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 20_000 });
  await screenContains('WASM_REMOTE_OK', 20_000);
  cli('gateway', 'off');
  await expect(page.getByRole('status')).not.toHaveText('Connected', { timeout: 15_000 });
  cli('gateway', 'on');
  await expect(page.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  await page.reload();
  await expect(page.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  await screenContains('WASM_REMOTE_OK', 20_000);
  await menu();
  await expect(page.getByRole('spinbutton', { name: 'Terminal font size' })).toHaveValue('16');
  await page.getByRole('checkbox', { name: 'Screen reader support', exact: true }).check();
  await closeMenu();
  await page.reload();
  await expect(page.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  await expect(terminal).toContainText('WASM_REMOTE_OK', { timeout: 20_000 });
  const hostId = decodeURIComponent(new URL(page.url()).hash.split('/')[2]!.split('?')[0]!);
  await page.evaluate((host) => sessionStorage.setItem(`vibeke-tui-pending:${host}`, '{broken'), hostId);
  await page.reload();
  await page.getByRole('button', { name: 'Review saved operations', exact: true }).click();
  await expect(page.getByRole('dialog')).toContainText('Check task status on the host');
  await page.getByRole('button', { name: 'Discard saved operations and reload', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  await expect(terminal).toContainText('WASM_REMOTE_OK', { timeout: 20_000 });
  cli('pane', 'send-text', pane, "i=0; while [ \"$i\" -lt 1000 ]; do printf 'burst: abcdefghijklmnopqrstuvwxyz 0123456789 café 日本語\\n'; i=$((i+1)); done; printf 'BURST_%s\\n' DONE");
  cli('pane', 'send-keys', pane, 'enter');
  await expect(terminal).toContainText('BURST_DONE', { timeout: 20_000 });
  const manifest = await (await fetch(`${origin}/tui/manifest.json`)).json() as { moduleUrl: string };
  const memory = await page.evaluate(async (url) => {
    const module = await import(url);
    const exports = await module.default();
    return exports.memory.buffer.byteLength;
  }, manifest.moduleUrl);
  console.log('WASM memory bytes:', memory);
  console.log('Startup measurements:', await page.evaluate(() => performance.getEntriesByName('vibeke-tui-initialize').map((e) => ({ durationMs: Math.round(e.duration) }))));
  await page.screenshot({ path: process.env.VIBEKE_TUI_SCREENSHOT ?? '/tmp/vibeke-wasm-tui.png' });
  // Real guest identities, existing invitations, and scoped WASM rendering.
  const privateWs = JSON.parse(cli('workspace', 'create', '--cwd', temp, '--command', '/bin/sh', '--name', 'PRIVATE_WORKSPACE_SENTINEL'));
  for (const [scope, workspaceShare] of [['view', false], ['approve', false], ['full', false], ['full', true]] as const) {
    const invitation = JSON.parse(cli('api', 'call', 'gateway.call', JSON.stringify({ method: 'share.create', params: { kind: 'share', scope, ...(workspaceShare ? { workspace: ws.workspace.id } : { pane }), ttl_s: 3600, label: `Guest ${scope}` } })));
    const guestContext = await browser.newContext({ viewport: { width: 1000, height: 700 } });
    guestContext.setDefaultTimeout(15_000);
    const guest = await guestContext.newPage();
    guest.on('pageerror', (e) => errors.push(String(e)));
    await guest.goto(invitation.link);
    await guest.getByRole('button', { name: 'Accept invitation', exact: true }).click();
    // The reconnect smoke above and every guest share one loopback IP. Respect the
    // relay's admission bucket by retrying only its transient pairing refusal.
    await expect.poll(async () => {
      if (await guest.getByRole('status').filter({ hasText: /^Connected$/ }).isVisible()) return true;
      if (await guest.getByRole('status').filter({ hasText: /Could not reach the host/ }).isVisible())
        await guest.getByRole('button', { name: 'Accept invitation', exact: true }).click();
      return false;
    }, { timeout: 90_000, intervals: [1000, 3000, 7000] }).toBe(true);
    await expect(guest.getByTestId('share-access')).toContainText(scope === 'full' ? 'Control' : scope === 'approve' ? 'View + approve' : 'View only');
    await guest.getByRole('button', { name: 'Browser menu', exact: true }).click();
    await expect(guest.getByRole('checkbox', { name: 'Open this terminal when I launch the app', exact: true })).toHaveCount(0);
    if (scope !== 'full') await expect(guest.getByRole('button', { name: 'Paste', exact: true })).toBeDisabled();
    await guest.getByRole('checkbox', { name: 'Screen reader support', exact: true }).check();
    await guest.getByRole('button', { name: 'Close', exact: true }).click();
    const guestTerminal = guest.locator('.xterm-accessibility-tree');
    await expect(guestTerminal).toContainText('WASM experiment', { timeout: 20_000 });
    await expect(guestTerminal).not.toContainText('PRIVATE_WORKSPACE_SENTINEL');
    if (scope === 'view') {
      await guest.getByRole('button', { name: 'Conversation', exact: true }).click();
      await guest.getByRole('button', { name: 'Open terminal', exact: true }).click();
      await expect(guest.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
    }
    const beforeSize = JSON.parse(cli('pane', 'get', pane)).pane;
    await guest.setViewportSize({ width: 700, height: 500 });
    // A guest cannot resize the PTY. Restore room for the owner's full terminal before
    // asserting prompt output; a smaller guest viewport crops that fixed-size screen.
    await guest.setViewportSize({ width: 1600, height: 1000 });
    await guest.locator('.xterm-helper-textarea').focus();
    await guest.keyboard.type(`printf 'GUEST_${scope}_%s\\n' OK`); await guest.keyboard.press('Enter');
    if (scope === 'full') await expect(guestTerminal).toContainText('GUEST_full_OK', { timeout: 20_000 });
    else {
      // An owner-generated barrier proves the guest's earlier input has had time to arrive.
      cli('pane', 'send-text', pane, `printf 'BARRIER_${scope}_%s\\n' OK`); cli('pane', 'send-keys', pane, 'enter');
      await expect(guestTerminal).toContainText(`BARRIER_${scope}_OK`, { timeout: 20_000 });
      expect(cli('pane', 'read', pane, '--lines', '200')).not.toContain(`GUEST_${scope}_OK`);
    }
    const afterSize = JSON.parse(cli('pane', 'get', pane)).pane;
    expect([afterSize.cols, afterSize.rows]).toEqual([beforeSize.cols, beforeSize.rows]);
    const all = JSON.parse(readFileSync(`${temp}/gateway/devices.json`, 'utf8'));
    const guestDevice = (Array.isArray(all) ? all : all.devices).find((d: { kind?: string; scope?: string }) => d.kind === 'share' && d.scope === scope);
    if (workspaceShare) await guest.clock.setFixedTime(new Date((guestDevice.expires_at + 1) * 1000));
    else cli('gateway', 'revoke', guestDevice.id);
    await expect(guest.getByRole('status')).toHaveText('This shared session has ended.', { timeout: 15_000 });
    await expect(guest.locator('.xterm')).toHaveCount(0);
    await guestContext.close();
    console.log(`Guest ${scope} ${workspaceShare ? 'workspace expiry' : 'pane revocation'} passed`);
  }
  expect(privateWs.root_pane.id).not.toBe(pane);
  console.log('PASS: guest View/Approve/Control shares, scoped model, owner geometry and revocation');
  const devices = JSON.parse(readFileSync(`${temp}/gateway/devices.json`, 'utf8'));
  const device = (Array.isArray(devices) ? devices : devices.devices)[0];
  cli('gateway', 'revoke', device.id);
  await expect(page.getByRole('status')).not.toHaveText('Connected', { timeout: 15_000 });
  expect(errors).toEqual([]);
  console.log('PASS: pairing, Rust WASM rendering, shell/Unicode input, palette, split, resize, reconnect, offline recovery, reload, preferences, two clients, visibility recovery, revocation');
} catch (e) {
  const page = browser.contexts().at(-1)?.pages()[0];
  if (page) {
    await page.screenshot({ path: '/tmp/vibeke-wasm-tui-failed.png' }).catch(() => {});
    console.error((await page.locator('body').innerText().catch(() => '')).slice(-5000));
  }
  console.error(logs.join('').slice(-8000));
  throw e;
} finally {
  await browser.close();
  try { cli('server', 'stop', '--kill-panes'); } catch {}
  staticServer?.closeAllConnections(); staticServer?.close();
  for (const child of children) child.kill('SIGTERM');
  if (process.env.VIBEKE_TUI_KEEP) console.log('Test files:', temp);
  else rmSync(temp, { recursive: true, force: true });
}
