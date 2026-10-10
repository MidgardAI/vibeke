// Real browser + Rust/WASM TUI + native server + encrypted local relay.
// Run from web/: bun apps/site/scripts/check-wasm-tui.ts
import { chromium, expect } from '@playwright/test';
import { spawn, execFileSync, type ChildProcess } from 'node:child_process';
import { mkdtempSync, rmSync, readFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { createServer } from 'node:net';
import { resolve } from 'node:path';

const root = resolve(import.meta.dir, '../../../..');
const binary = process.env.VIBEKE_TEST_BINARY ?? resolve(root, 'target/debug/vibeke');
const temp = mkdtempSync('/tmp/vkw-');
const env = { ...process.env, VIBEKE_RUNTIME_DIR: `${temp}/run`, VIBEKE_STATE_DIR: `${temp}/state`, VIBEKE_CONFIG: `${temp}/config.toml`, VIBEKE_GATEWAY_DIR: `${temp}/gateway` };
for (const name of ['VIBEKE', 'VIBEKE_SOCKET', 'VIBEKE_SESSION', 'VIBEKE_PANE_TOKEN']) delete env[name as keyof typeof env];
const children: ChildProcess[] = [];
let staticServer: ReturnType<typeof Bun.serve> | undefined;
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
const browser = await chromium.launch({ headless: true });
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
    staticServer = Bun.serve({ hostname: '127.0.0.1', port: Number(new URL(origin).port), async fetch(request) {
      const path = resolve(output, 'static', '.' + decodeURIComponent(new URL(request.url).pathname));
      if (!path.startsWith(`${output}/static/`)) {
        if (new URL(request.url).pathname === '/') return new Response(Bun.file(`${output}/static/index.html`), { headers });
        return new Response('Not found', { status: 404 });
      }
      const file = Bun.file(path);
      return await file.exists() ? new Response(file, { headers }) : new Response('Not found', { status: 404 });
    } });
  } else {
    start(process.execPath, ['run', '--cwd', `${root}/web/apps/pwa`, 'dev', '--host', '127.0.0.1', '--port', new URL(origin).port, '--strictPort'], { VIBEKE_WASM_TUI: '1' });
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
  const context = await browser.newContext({ viewport: { width: 1280, height: 850 }, locale: 'en-US' });
  const page = await context.newPage();
  const errors: string[] = [];
  page.on('pageerror', (e) => errors.push(e.stack ?? String(e)));
  page.on('console', (m) => { if (m.type() === 'error') logs.push(m.text()); });
  await page.goto(link);
  await page.getByRole('button', { name: 'Pair', exact: true }).click();
  await expect(page.getByRole('button', { name: 'Open Vibeke', exact: true })).toBeVisible({ timeout: 30_000 });
  console.log('Browser paired');
  await page.getByRole('button', { name: 'Open Vibeke', exact: true }).click();
  await page.goto(`${origin}/#/settings/system`);
  await page.getByRole('button', { name: 'Open TUI', exact: true }).click();
  await expect(page.getByRole('status').filter({ hasText: /^Connected$/ })).toBeVisible({ timeout: 30_000 });
  console.log('TUI connected');
  const terminal = page.locator('.xterm-accessibility-tree');
  const screenContains = async (text: string, timeout = 20_000) => {
    await page.getByRole('button', { name: 'Screen reader', exact: true }).click();
    await expect(terminal).toContainText(text, { timeout });
    await page.getByRole('button', { name: 'Screen reader', exact: true }).click();
  };
  await screenContains('WASM experiment', 20_000);
  await page.locator('.xterm-helper-textarea').focus();
  await page.keyboard.type("printf 'WASM_REMOTE_%s\\n' 'OK'\n");
  await screenContains('WASM_REMOTE_OK', 20_000);
  // Browser text insertion must preserve multi-byte characters.
  await page.keyboard.insertText("printf 'Unicode: café 日本語\\n'");
  await page.keyboard.press('Enter');
  await screenContains('Unicode: café 日本語', 20_000);
  expect(JSON.parse(cli('pane', 'read', pane, '--source', 'screen', '--lines', '100')).text).toContain('WASM_REMOTE_OK');
  await page.getByRole('button', { name: 'Commands', exact: true }).click();
  await screenContains('command palette', 5000);
  await page.keyboard.press('Escape');
  await page.keyboard.press('Control+b');
  await page.keyboard.press('v');
  await expect.poll(() => JSON.parse(cli('pane', 'list')).panes.length, { timeout: 15_000 }).toBe(2);
  await page.setViewportSize({ width: 1000, height: 700 });
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
  await page.screenshot({ path: process.env.VIBEKE_TUI_SCREENSHOT ?? '/tmp/vibeke-wasm-tui.png' });
  const devices = JSON.parse(readFileSync(`${temp}/gateway/devices.json`, 'utf8'));
  const device = (Array.isArray(devices) ? devices : devices.devices)[0];
  cli('gateway', 'revoke', device.id);
  await expect(page.getByRole('status')).not.toHaveText('Connected', { timeout: 15_000 });
  expect(errors).toEqual([]);
  console.log('PASS: pairing, Rust WASM rendering, shell/Unicode input, palette, split, resize, reconnect, offline recovery, reload, revocation');
} catch (e) {
  const page = browser.contexts()[0]?.pages()[0];
  if (page) {
    await page.screenshot({ path: '/tmp/vibeke-wasm-tui-failed.png' }).catch(() => {});
    console.error((await page.locator('body').innerText().catch(() => '')).slice(-5000));
  }
  console.error(logs.join('').slice(-8000));
  throw e;
} finally {
  await browser.close();
  try { cli('server', 'stop', '--kill-panes'); } catch {}
  staticServer?.stop(true);
  for (const child of children) child.kill('SIGTERM');
  if (process.env.VIBEKE_TUI_KEEP) console.log('Test files:', temp);
  else rmSync(temp, { recursive: true, force: true });
}
