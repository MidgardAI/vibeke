// Smoke: the built app launches, shows the pairing screen (no hosts yet) with "Connect to this
// Mac", answers the keyboard (⌘K palette, ? cheat sheet), and quits cleanly.

import { rmSync } from 'node:fs';
import { expect, test } from '@playwright/test';
import { built, hasDisplay, launchApp, shortTmp } from './helpers';

type Bridge = { vibeke: { invoke(c: string, ...a: unknown[]): Promise<{ ok: boolean; error?: { message: string } }> } };

test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'run `bun run build` first');

test('launches to the pairing screen and quits cleanly', async () => {
  // An empty gateway dir and a missing CLI: nothing local to connect to.
  const gw = shortTmp('vkgw-');
  const t0 = Date.now();
  const a = await launchApp({ HOME: gw, VIBEKE_GATEWAY_DIR: gw, VIBEKE_BIN: '/nonexistent/vibeke', PATH: '/usr/bin:/bin' });
  try {
    const { page, app } = a;
    await expect(page.getByText('Pair with a host')).toBeVisible();
    console.log(`launch → pairing screen interactive: ${Date.now() - t0} ms (includes Playwright attach)`);
    await expect(page.getByRole('button', { name: /Connect to this (Mac|computer)/ })).toBeVisible();
    await expect(page.getByPlaceholder('https://…/#/pair?d=…')).toBeVisible();

    // The renderer is sandboxed: no Node, only the narrow bridge.
    const env = await page.evaluate(() => ({
      require: typeof (globalThis as { require?: unknown }).require,
      process: typeof (globalThis as { process?: unknown }).process,
      bridge: Object.keys((window as unknown as { vibeke: object }).vibeke).sort(),
    }));
    expect(env).toEqual({ require: 'undefined', process: 'undefined', bridge: ['invoke', 'on'] });

    // A renderer can neither choose the executable nor the update feed (P1): both are refused.
    const refused = await page.evaluate(async () => {
      const b = (window as unknown as Bridge).vibeke;
      const tries = [{ vibekePath: '/tmp/evil' }, { updateFeed: 'https://evil.example/' }, { notifications: false }];
      const out: string[] = [];
      for (const t of tries) out.push(await b.invoke('vk:settings.set', t).then((r) => (r.ok ? 'ok' : 'refused'), () => 'refused'));
      const s = (await b.invoke('vk:settings.get')) as unknown as { value: Record<string, unknown> };
      return { out, keys: Object.keys(s.value).sort(), path: s.value.vibekePath };
    });
    expect(refused.out).toEqual(['refused', 'refused', 'ok']);
    expect(refused.keys).not.toContain('updateFeed');
    expect(refused.path).toBe('');

    // Command palette and cheat sheet.
    await page.keyboard.press(process.platform === 'darwin' ? 'Meta+K' : 'Control+K');
    await expect(page.getByRole('dialog', { name: 'Command palette' })).toBeVisible();
    await page.keyboard.type('settings');
    await page.keyboard.press('Enter');
    await expect(page).toHaveURL(/#\/settings$/);
    await expect(page.getByText('Desktop', { exact: true })).toBeVisible();
    await page.keyboard.press('?');
    await expect(page.getByRole('dialog', { name: 'Keyboard shortcuts' })).toBeVisible();
    await page.keyboard.press('Escape');

    // "Connect to this Mac" without a CLI explains what is missing.
    await page.keyboard.press('Escape');
    await page.evaluate(() => (location.hash = '#/pair'));
    await page.getByRole('button', { name: /Connect to this (Mac|computer)/ }).click();
    await expect(page.getByText(/command was not found/)).toBeVisible();

    await page.screenshot({ path: 'test-results/smoke-pair.png' });
    const pid = app.process().pid;
    await a.close();
    // Quit really quits (the window close only hides; app.quit must end the process).
    expect(pid).toBeTruthy();
    await expect.poll(() => {
      try {
        process.kill(pid!, 0);
        return 'alive';
      } catch {
        return 'gone';
      }
    }).toBe('gone');
  } finally {
    await a.close();
    rmSync(gw, { recursive: true, force: true });
  }
});

test('a deep link opened at launch is delivered after the renderer is ready', async () => {
  const gw = shortTmp('vkgw-');
  const a = await launchApp({ HOME: gw, VIBEKE_GATEWAY_DIR: gw, VIBEKE_BIN: '/nonexistent/vibeke', PATH: '/usr/bin:/bin' }, ['vibeke://inbox']);
  try {
    await expect(a.page).toHaveURL(/#\/inbox$/, { timeout: 15_000 });
  } finally {
    await a.close();
    rmSync(gw, { recursive: true, force: true });
  }
});
