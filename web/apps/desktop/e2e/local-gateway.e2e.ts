// Integration (spec 16 §16.3): a real Vibeke server + gateway in temp dirs → "Connect to this Mac"
// pairs over the gateway's Unix socket → the dashboard shows the session → approvals opened by
// Claude-style hooks in panes reach the Inbox → keyboard answers land on the card the user is on
// (focus follows, held keys and auto-advance never answer) → the hooks get their decisions. Also
// the modal palette (focus trap, inert background, focus restored), the quick-approvals popover,
// a popped-out pane window bound to its pane, and light/dark screenshots of each surface.

import { expect, test, type Page } from '@playwright/test';
import { TestHost, built, hasDisplay, launchApp, settled, shoot, vibekeBin, type LaunchedApp } from './helpers';

const bin = vibekeBin();
test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'run `bun run build` first');
test.skip(!bin, 'build the CLI first: cargo build -p vibeke (or set VIBEKE_BIN)');

let host: TestHost;
let a: LaunchedApp | null = null;
const mod = process.platform === 'darwin' ? 'Meta' : 'Control';

test.beforeAll(async () => {
  host = new TestHost(bin!);
  await host.start();
});

test.afterAll(async () => {
  await a?.close();
  await host?.stop();
});

const status = (needle: string) => host.interactions('all').find((i) => i.title.includes(needle));

/** Focus stays inside the open dialog however often Tab is pressed. */
async function expectFocusTrapped(page: Page, dialog: string) {
  for (let i = 0; i < 6; i++) {
    await page.keyboard.press(i % 2 ? 'Shift+Tab' : 'Tab');
    expect(await page.evaluate((name) => !!document.activeElement?.closest(`[aria-label="${name}"], [aria-labelledby]`), dialog)).toBe(true);
  }
}

test('connect to this Mac, keyboard approvals, palette, popover, pane window', async () => {
  test.setTimeout(180_000);
  const wsA = host.workspace('alpha');
  const wsB = host.workspace('beta');
  a = await launchApp({ ...host.env, VIBEKE_BIN: bin!, HOME: process.env.HOME ?? host.env.HOME! });
  const { page, app } = a;
  await page.setViewportSize?.({ width: 1280, height: 800 }).catch(() => {});

  // One click: `vibeke gateway pair --local` + pairing over the local socket.
  await page.getByRole('button', { name: /Connect to this (Mac|computer)/ }).click();
  await expect(page.getByText(/^Connected to /)).toBeVisible({ timeout: 30_000 });
  await page.getByRole('button', { name: 'Open Vibeke' }).click();
  // First run shows the tour (a modal dialog); skip it.
  await page.getByRole('button', { name: 'Skip' }).click();

  // Dashboard: the host is online and both workspaces are listed in the sidebar; ⌘2 opens the
  // first one.
  const workspaces = page.getByRole('navigation', { name: 'Workspaces' }).locator('[data-nav-item]');
  await expect(workspaces).toHaveCount(2);
  await page.keyboard.press(`${mod}+2`);
  await expect(page).toHaveURL(/#\/w\/[^/]+\/[^/?]+/);
  await shoot(app, page, 'main-workspace');

  // Two approvals from two panes → two Inbox cards.
  host.requestApproval(wsA.pane, 'echo hello-alpha', wsA.cwd);
  host.requestApproval(wsB.pane, 'echo hello-beta', wsB.cwd);
  await page.keyboard.press(`${mod}+1`);
  const cardA = page.locator('[data-nav-item]').filter({ hasText: 'echo hello-alpha' });
  const cardB = page.locator('[data-nav-item]').filter({ hasText: 'echo hello-beta' });
  await expect(cardA).toBeVisible({ timeout: 30_000 });
  await expect(cardB).toBeVisible({ timeout: 30_000 });
  await shoot(app, page, 'main-inbox');

  // Command palette: modal (background inert, Tab stays inside), Escape restores focus.
  const opener = page.locator('main');
  await opener.click({ position: { x: 5, y: 5 } });
  await page.keyboard.press(`${mod}+K`);
  const palette = page.getByRole('dialog', { name: 'Command palette' });
  await expect(palette).toBeVisible();
  await expect(page.getByRole('combobox', { name: /Jump to a pane/ })).toBeFocused();
  expect(await page.evaluate(() => document.getElementById('root')!.hasAttribute('inert'))).toBe(true);
  await expectFocusTrapped(page, 'Command palette');
  await shoot(app, page, 'palette');
  await page.keyboard.press('Escape');
  await expect(palette).toHaveCount(0);
  expect(await page.evaluate(() => document.getElementById('root')!.hasAttribute('inert'))).toBe(false);

  // The menu-bar popover shows the same cards.
  await page.evaluate(() => (window as unknown as { vibeke: { invoke(c: string, o: unknown): Promise<unknown> } }).vibeke.invoke('vk:window', { op: 'quick' }));
  const quick = await app.waitForEvent('window', { predicate: (p) => p.url().includes('surface=quick'), timeout: 15_000 });
  await expect(quick.locator('[data-nav-item]').filter({ hasText: 'echo hello-alpha' })).toBeVisible();
  await shoot(app, quick, 'quick-popover');
  await quick.keyboard.press('Escape');

  // Keyboard: select A with j, then move focus into B → `a` acts on B (focus wins), a held key
  // answers once.
  await page.bringToFront();
  await opener.click({ position: { x: 5, y: 5 } });
  await page.keyboard.press('j');
  const first = (await cardA.getAttribute('data-selected')) !== null ? cardA : cardB;
  const other = first === cardA ? cardB : cardA;
  const otherNeedle = other === cardA ? 'hello-alpha' : 'hello-beta';
  const firstNeedle = other === cardA ? 'hello-beta' : 'hello-alpha';
  await other.locator('[data-act="deny"]').focus();
  await expect(other).toHaveAttribute('data-selected', '');
  await page.locator('body').evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.keyboard.down('a');
  await page.keyboard.down('a'); // auto-repeat
  await page.keyboard.down('a');
  await page.keyboard.up('a');
  await expect.poll(() => status(otherNeedle)?.status, { timeout: 20_000 }).not.toBe('Open');
  expect(status(otherNeedle)!.answer?.decision?.toLowerCase()).toBe('allow');
  await expect(other).toHaveCount(0, { timeout: 20_000 });
  // The successor is only highlighted: the next `a` confirms the selection, it does not answer.
  await expect(first).toHaveAttribute('data-selected', '');
  await page.keyboard.press('a');
  await new Promise((r) => setTimeout(r, 1500));
  expect(status(firstNeedle)?.status).toBe('Open');
  await page.keyboard.press('a');
  await expect.poll(() => status(firstNeedle)?.status, { timeout: 20_000 }).not.toBe('Open');
  await expect(first).toHaveCount(0, { timeout: 20_000 });
  await settled(page);
  await shoot(app, page, 'main-inbox-empty');

  // Settings at a comfortable width.
  await page.keyboard.press(`${mod}+K`);
  await page.keyboard.type('settings');
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/#\/settings$/);
  await shoot(app, page, 'main-settings');

  // Pop the pane out: bound to its pane (no previous/next), no Lock without a resume path.
  await workspaces.first().click();
  await expect(page).toHaveURL(/#\/w\/[^/]+\/[^/?]+/);
  await page.getByRole('button', { name: 'Open this pane in a new window' }).click();
  const paneWin = await app.waitForEvent('window', { predicate: (p) => p.url().includes('surface=pane'), timeout: 15_000 });
  await expect(paneWin.getByText('HOOK-DONE').first()).toBeVisible({ timeout: 20_000 });
  await expect(paneWin.getByRole('button', { name: /^(Previous|Next) pane$/ })).toHaveCount(0);
  await shoot(app, paneWin, 'pane-window');
  await paneWin.keyboard.press(`${mod}+K`);
  await paneWin.keyboard.type('lock');
  await expect(paneWin.getByRole('option', { name: /Lock/ })).toHaveCount(0);
  await paneWin.keyboard.press('Escape');
});
