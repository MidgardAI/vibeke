// Focused regressions against the compiled WASM module in a real browser.
// Run from web/: bun apps/site/scripts/check-wasm-tui-module.ts
import { chromium, firefox, webkit, expect } from '@playwright/test';
import { execFileSync } from 'node:child_process';
import { readFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const root = resolve(fileURLToPath(new URL('../../../../', import.meta.url)));
const fixtures = JSON.parse(execFileSync('mise', ['exec', '--', 'cargo', 'run', '--quiet', '-p', 'vk-tui', '--example', 'browser_fixtures'], { cwd: root, maxBuffer: 4 * 1024 * 1024 }).toString());
const assets = resolve(root, 'web/apps/pwa/public');
const manifest = JSON.parse(readFileSync(`${assets}/tui/manifest.json`, 'utf8'));
const server = Bun.serve({ hostname: '127.0.0.1', port: 0, fetch(request) {
  const path = new URL(request.url).pathname;
  return path === '/' ? new Response('<!doctype html><body>Browser TUI regressions</body>', { headers: { 'Content-Type': 'text/html' } }) : new Response(Bun.file(assets + path));
} });
const engine = process.env.VIBEKE_TUI_BROWSER ?? 'chromium';
const browser = await ({ chromium, firefox, webkit }[engine] ?? chromium).launch({ headless: true });
try {
  const page = await browser.newPage();
  await page.goto(server.url.href);
  const result = await page.evaluate(async ({ fixtures, url }) => {
    const module = await import(url);
    await module.default();
    const calls = (tui: any) => {
      const bytes: Uint8Array = tui.outgoing();
      const commands = [];
      for (let at = 0; at < bytes.length;) {
        const end = at + 4 + new DataView(bytes.buffer, bytes.byteOffset + at, 4).getUint32(0, true);
        let offset = at + 4;
        const uint = () => {
          let value = 0, shift = 0, byte;
          do { byte = bytes[offset++]; value += (byte & 127) * 2 ** shift; shift += 7; } while (byte & 128);
          return value;
        };
        if (uint() === fixtures.commandTag) {
          uint();
          const size = uint();
          commands.push(JSON.parse(new TextDecoder().decode(bytes.slice(offset, offset + size))));
        }
        at = end;
      }
      return commands;
    };
    const reply = (tui: any, req: number, result: unknown) => {
      const uint = (value: number): number[] => value < 128 ? [value] : [(value % 128) + 128, ...uint(Math.floor(value / 128))];
      const text = new TextEncoder().encode(JSON.stringify({ result }));
      const body = [fixtures.resultTag, ...uint(req), ...uint(text.length), ...text];
      const bytes = new Uint8Array(4 + body.length);
      new DataView(bytes.buffer).setUint32(0, body.length, true);
      bytes.set(body, 4);
      tui.receive(bytes);
    };
    const create = (features: string[], host = 'regression') => {
      const tui = new module.BrowserTui('Renamed host', host);
      tui.resize(120, 40, 9, 18, 100);
      tui.connected('regression-client', JSON.stringify(features));
      tui.receive(new Uint8Array(fixtures.model));
      tui.tick();
      return tui;
    };
    const owner = create([]);
    calls(owner);
    owner.action('preview_list');
    const previewBytes = owner.render().length; // SystemTime used to trap here.
    owner.key('Escape', 0, false, false);
    owner.action('help');
    const stillUsable = owner.render().length > 0;
    owner.free();

    const guest = create(['shared_tui', 'shared_tui.approve']);
    const startup = calls(guest);
    guest.action('batch_approvals');
    guest.key('y', 0, false, false);
    const approvals = calls(guest).filter((c) => c.method === 'interaction.answer');
    guest.free();
    const viewer = create(['shared_tui']);
    calls(viewer);
    viewer.action('batch_approvals');
    viewer.key('y', 0, false, false);
    const viewOnlyCalls = calls(viewer);
    viewer.free();

    const key = 'vibeke-tui-pending:storage-test';
    sessionStorage.setItem(key, JSON.stringify([{ key: 'pending-1', method: 'task.track', params: {}, machine: 'Old name', created_at_ms: 1 }]));
    const resumed = create([], 'storage-test');
    const recovery = calls(resumed).find((c) => c.method === 'task.operation.get');
    if (recovery) reply(resumed, recovery.id, { known: true, result: {} });
    const saved = sessionStorage.getItem(key);
    resumed.free();
    sessionStorage.setItem(key, 'invalid JSON');
    let corruptRejected = false;
    try { new module.BrowserTui('Host', 'storage-test'); } catch { corruptRejected = true; }
    const corruptPreserved = sessionStorage.getItem(key) === 'invalid JSON';
    return { previewBytes, stillUsable, startup, approvals, viewOnlyCalls, recovery, saved, corruptRejected, corruptPreserved };
  }, { fixtures, url: manifest.moduleUrl });
  expect(result.previewBytes).toBeGreaterThan(0);
  expect(result.stillUsable).toBe(true);
  expect(result.startup).toEqual([]);
  expect(result.approvals.map((c: any) => [c.params.interaction, c.params.decision_rev])).toEqual([['i1', 3], ['i2', 7]]);
  expect(result.viewOnlyCalls).toEqual([]);
  expect(result.recovery?.params.idempotency_key).toBe('pending-1');
  expect(result.saved).toBe('[]');
  expect(result.corruptRejected && result.corruptPreserved).toBe(true);
  console.log(`PASS (${engine}): previews, shared startup, batch revisions, view-only actions, storage reload/save and corruption recovery`);
} finally {
  await browser.close();
  server.stop();
}
