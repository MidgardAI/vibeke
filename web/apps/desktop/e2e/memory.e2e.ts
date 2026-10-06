// Footprint (spec 16 §16.3: < 250 MB working set with three hosts): three isolated real servers +
// gateways in temp dirs, paired over their local sockets, then the app's summed working set is
// measured with the main window shown, after the quick popover was used, and with every window
// closed (menu bar only), before and after hidden renderers are released. Prints working set and
// physical footprint per process type; the budget is asserted on the footprint.

import { execFileSync } from 'node:child_process';
import { expect, test } from '@playwright/test';
import type { ElectronApplication } from '@playwright/test';
import { TestHost, built, hasDisplay, launchApp, vibekeBin, type LaunchedApp } from './helpers';

const bin = vibekeBin();
test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'run `bun run build` first');
test.skip(!bin, 'build the CLI first: cargo build -p vibeke (or set VIBEKE_BIN)');

const BUDGET_MB = 250;
const hosts: TestHost[] = [];
let a: LaunchedApp | null = null;

test.afterAll(async () => {
  await a?.close();
  for (const h of hosts) await h.stop();
});

interface Sample {
  /** Summed working set (MB): counts the shared Electron framework pages once per process. */
  total: number;
  /** Summed physical footprint (MB, macOS `footprint`: what Activity Monitor shows), or -1. */
  footprint: number;
  byType: string;
}

/** phys_footprint of a pid in KB (macOS), or null. */
function physFootprint(pid: number): number | null {
  if (process.platform !== 'darwin') return null;
  try {
    const out = execFileSync('footprint', [String(pid)], { encoding: 'utf8', timeout: 10_000, stdio: ['ignore', 'pipe', 'ignore'] });
    const m = /phys_footprint:\s*([\d.]+)\s*(KB|MB|GB)/.exec(out);
    if (!m) return null;
    return Number(m[1]) * (m[2] === 'GB' ? 1024 * 1024 : m[2] === 'MB' ? 1024 : 1);
  } catch {
    return null;
  }
}

async function sample(app: ElectronApplication): Promise<Sample> {
  const m = await app.evaluate(({ app: x }) => x.getAppMetrics().map((p) => ({ pid: p.pid, type: p.type, ws: p.memory.workingSetSize })));
  const total = Math.round(m.reduce((s, p) => s + p.ws, 0) / 1024);
  const fp = m.map((p) => physFootprint(p.pid));
  const footprint = fp.every((x) => x !== null) ? Math.round(fp.reduce((s, x) => s! + x!, 0)! / 1024) : -1;
  const byType = m.map((p, i) => `${p.type} ${Math.round(p.ws / 1024)}${fp[i] !== null ? `/${Math.round(fp[i]! / 1024)}` : ''}`).join(', ');
  return { total, footprint, byType };
}

const settle = (ms: number) => new Promise((r) => setTimeout(r, ms));

test('three hosts stay within the memory budget', async () => {
  test.setTimeout(240_000);
  for (let i = 0; i < 3; i++) {
    const h = new TestHost(bin!);
    await h.start();
    h.workspace(`ws${i}`);
    hosts.push(h);
  }
  const t0 = Date.now();
  // Short release delays (defaults: popover 60 s, main window 10 min hidden).
  a = await launchApp({ VIBEKE_BIN: bin!, VIBEKE_POPOVER_TTL_MS: '3000', VIBEKE_MAIN_TTL_MS: '4000' });
  const { page, app } = a;
  await expect(page.getByText('Pair with a host')).toBeVisible();
  const startup = Date.now() - t0;

  const report: string[] = [`startup → pairing screen: ${startup} ms (includes Playwright attach)`];
  const idle = await sample(app);
  report.push(`0 hosts, main window: ${idle.total} MB ws, ${idle.footprint} MB footprint (${idle.byType})`);

  for (const [i, h] of hosts.entries()) {
    // `vibeke gateway pair --local` for that host, then pair over its socket through the bridge.
    const out = JSON.parse(h.run(['gateway', 'pair', '--local']).trim().split('\n').filter((l) => l.startsWith('{')).pop()!);
    const r = await page.evaluate(
      ([d, n]) => (window as unknown as { vibeke: { invoke(c: string, ...a: unknown[]): Promise<{ ok: boolean }> } }).vibeke.invoke('vk:pair', crypto.randomUUID(), d, n),
      [out.d as string, `e2e-${i}`],
    );
    expect(r.ok).toBe(true);
    if (i === 0) {
      await settle(6000);
      const one = await sample(app);
      report.push(`1 host, main window: ${one.total} MB ws, ${one.footprint} MB footprint (${one.byType})`);
    }
  }
  await page.evaluate(() => (location.hash = '#/panes'));
  await expect(page.locator('[data-nav-item]')).toHaveCount(3, { timeout: 30_000 });
  await settle(6000);
  const three = await sample(app);
  report.push(`3 hosts, main window: ${three.total} MB ws, ${three.footprint} MB footprint (${three.byType})`);

  // Quick popover once, then hidden.
  await page.evaluate(() => (window as unknown as { vibeke: { invoke(c: string, ...a: unknown[]): Promise<unknown> } }).vibeke.invoke('vk:window', { op: 'quick' }));
  const quick = await app.waitForEvent('window', { predicate: (p) => p.url().includes('surface=quick'), timeout: 15_000 });
  await quick.waitForLoadState('domcontentloaded');
  await settle(2000);
  const withQuick = await sample(app);
  report.push(`3 hosts, main + quick popover: ${withQuick.total} MB ws, ${withQuick.footprint} MB footprint`);
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows().find((w) => w.webContents.getURL().includes('surface=quick'))?.hide());

  // Every window closed: menu bar only.
  await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows().forEach((w) => w.close()));
  await settle(8000);
  const closed = await sample(app);
  report.push(`3 hosts, all windows closed: ${closed.total} MB ws, ${closed.footprint} MB footprint (${closed.byType})`);
  await settle(6000); // past both release delays
  const released = await sample(app);
  report.push(`3 hosts, menu bar only (hidden renderers released): ${released.total} MB ws, ${released.footprint} MB footprint (${released.byType})`);
  // The hidden main window and popover were released (recreated on demand).
  expect(await app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows().length)).toBe(0);
  // Reopening recreates the main window.
  const t1 = Date.now();
  await app.evaluate(({ app: x }) => x.emit('activate'));
  const again = await app.waitForEvent('window', { predicate: (p) => p.url().includes('surface=full'), timeout: 15_000 });
  await expect(again.locator('[data-nav-item]')).toHaveCount(3, { timeout: 15_000 });
  report.push(`reopen main window after release → panes listed: ${Date.now() - t1} ms`);
  console.log(`memory MB (per process: working set/footprint)\n  ${report.join('\n  ')}`);

  // Budget on physical footprint (stable between runs; the summed working set double-counts the
  // Electron framework pages every process maps and swings ±50 MB with system memory pressure).
  if (process.env.VIBEKE_E2E_MEMORY_BUDGET !== '0' && three.footprint >= 0) {
    expect(three.footprint, 'three hosts, main window (footprint)').toBeLessThan(BUDGET_MB);
    expect(released.footprint, 'three hosts, menu bar only (footprint)').toBeLessThan(BUDGET_MB);
  }
});
