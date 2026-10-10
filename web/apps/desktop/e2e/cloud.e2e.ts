// Cloud sandboxes (spec 17 §5–§8) against the server's `fake` provider: the Sandboxes screen lists
// the providers with their sign-in state, the sign-in form (masked token field) signs in to Fake,
// "Send to cloud…" moves a shell pane with uncommitted work into a new box, the box shows as
// running and attached, "Bring back from cloud…" brings the work back to this host and leaves the
// box paused and clean, and a send whose sign-in went away meanwhile (`needs_auth`) opens the
// sign-in prompt and then goes on. The credential lives in a file keychain under the test root.

import { execFileSync } from 'node:child_process';
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { TestHost, built, hasDisplay, launchApp, shoot, vibekeBin, type LaunchedApp } from './helpers';

const bin = vibekeBin();
test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'run `bun run build` first');
test.skip(!bin, 'build the CLI first: cargo build -p vibeke (or set VIBEKE_BIN)');

let host: TestHost;
let a: LaunchedApp | null = null;
const mod = process.platform === 'darwin' ? 'Meta' : 'Control';
const FAKE = 'Fake (tests)';

test.beforeAll(async () => {
  host = new TestHost(bin!);
  const fake = join(host.root, 'fake');
  mkdirSync(fake, { recursive: true });
  host.env.VIBEKE_CLOUD_FAKE_DIR = fake;
  for (const k of ['GIT_AUTHOR_NAME', 'GIT_COMMITTER_NAME']) host.env[k] = 't';
  for (const k of ['GIT_AUTHOR_EMAIL', 'GIT_COMMITTER_EMAIL']) host.env[k] = 't@example.com';
  const cfg = join(host.env.XDG_CONFIG_HOME!, 'vibeke');
  mkdirSync(cfg, { recursive: true });
  writeFileSync(join(cfg, 'config.toml'), `[security]\nkeychain = "file:${join(host.root, 'keychain.json')}"\n\n[cloud]\ndefault_provider = "fake"\n`);
  writeFileSync(join(host.env.HOME!, '.gitconfig'), '[user]\n\tname = t\n\temail = t@example.com\n');
  await host.start();
});

test.afterAll(async () => {
  await a?.close();
  await host?.stop();
});

const git = (cwd: string, args: string[]) => execFileSync('git', args, { cwd, env: host.env, encoding: 'utf8' }).trim();

function api<T = Record<string, unknown>>(method: string, params: unknown = {}): T {
  return JSON.parse(host.cli(['api', 'call', method, JSON.stringify(params)])) as T;
}

interface Job {
  id: string;
  direction: string;
  state: string;
  error?: { message?: string };
  result?: { pane?: string; box?: string; worktree?: string };
}
const jobs = () => api<{ jobs: Job[] }>('cloud.jobs').jobs;

async function palette(page: Page, text: string, option: string | RegExp) {
  await page.keyboard.press(`${mod}+K`);
  const dialog = page.getByRole('dialog', { name: 'Command palette' });
  await expect(dialog).toBeVisible();
  // Keys typed before the field has focus would go to the terminal.
  const field = page.getByRole('combobox', { name: /Jump to a pane/ });
  await expect(field).toBeFocused();
  await field.fill(text);
  await dialog.getByRole('option', { name: option }).first().click();
  await expect(dialog).toHaveCount(0);
}

test('sign in, send a pane to the cloud, bring it back, sign in again on needs_auth', async () => {
  test.setTimeout(240_000);

  // A repository with a commit, opened as a workspace, and work that is not committed yet.
  const repo = join(host.root, 'repo');
  mkdirSync(repo, { recursive: true });
  git(repo, ['init', '-q', '-b', 'main']);
  writeFileSync(join(repo, 'a.txt'), 'hello\n');
  git(repo, ['add', 'a.txt']);
  git(repo, ['commit', '-qm', 'a']);
  const ws = JSON.parse(host.cli(['workspace', 'create', '--cwd', repo, '--name', 'alpha']));
  const pane = ws.root_pane.handle as string;
  writeFileSync(join(repo, 'wip.txt'), 'uncommitted work\n');

  a = await launchApp({ ...host.env, VIBEKE_BIN: bin!, HOME: process.env.HOME ?? host.env.HOME! });
  const { page, app } = a;

  await page.getByRole('button', { name: /Connect to this (Mac|computer)/ }).click();
  await expect(page.getByText(/^Connected to /)).toBeVisible({ timeout: 30_000 });
  await page.getByRole('button', { name: 'Open Vibeke' }).click();
  await page.getByRole('button', { name: 'Skip' }).click();
  await expect(page.getByRole('navigation', { name: 'Workspaces' }).locator('[data-nav-item]')).toHaveCount(1);

  // Sandboxes from Settings: every provider, none signed in.
  await page.keyboard.press(`${mod}+4`);
  await expect(page).toHaveURL(/#\/settings$/);
  const row = page.getByText('Cloud machines that run your tasks.', { exact: false }).locator('xpath=../..');
  await row.getByRole('button', { name: 'Open' }).click();
  await expect(page).toHaveURL(/#\/sandboxes$/);
  for (const label of ['Fly.io Sprites', 'E2B', FAKE]) {
    await expect(page.getByRole('region', { name: label })).toBeVisible({ timeout: 20_000 });
  }
  const fake = page.getByRole('region', { name: FAKE });
  await expect(fake.getByText('Not signed in')).toBeVisible();
  await shoot(app, page, 'cloud-sandboxes-signed-out');

  // Sign in to Fake with the form: the token field is masked.
  await fake.getByRole('button', { name: 'Sign in' }).click();
  const signIn = page.getByRole('dialog', { name: `Sign in to ${FAKE}` });
  await expect(signIn).toBeVisible();
  const token = signIn.getByLabel('Fake token');
  await expect(token).toHaveAttribute('type', 'password');
  await token.fill('fake-token');
  await shoot(app, page, 'cloud-sign-in');
  await signIn.getByRole('button', { name: 'Save' }).click();
  await expect(signIn).toHaveCount(0);
  await expect(fake.getByText(/^Signed in/)).toBeVisible({ timeout: 15_000 });
  await expect(fake.getByRole('button', { name: 'Sign out' })).toBeVisible();
  // The token went to the configured file keychain.
  expect(readFileSync(join(host.root, 'keychain.json'), 'utf8')).toContain('fake');

  // Send the workspace's pane to a new box.
  await page.keyboard.press(`${mod}+2`);
  await expect(page).toHaveURL(/#\/w\/[^/]+\/[^/?]+/);
  // The workspace header offers the move for a host pane.
  await page.getByRole('banner').getByRole('button', { name: 'Cloud', exact: true }).click();
  const send = page.getByRole('dialog', { name: 'Send to cloud' });
  await expect(send).toBeVisible();
  await expect(send.getByRole('button', { name: /Fly\.io Sprites/ })).toBeVisible({ timeout: 20_000 });
  await shoot(app, page, 'cloud-send-provider');
  await send.getByRole('button', { name: new RegExp(FAKE.replace(/[()]/g, '\\$&')) }).click();
  await send.getByRole('button', { name: /A new sandbox/ }).click();
  await send.getByRole('button', { name: `Send to ${FAKE}` }).click();
  await expect(send.getByText('Done.', { exact: true })).toBeVisible({ timeout: 120_000 });
  await shoot(app, page, 'cloud-send-done');
  const sent = jobs().find((j) => j.direction === 'send' && j.state === 'done');
  expect(sent?.result?.pane).toBeTruthy();
  const boxRef = sent!.result!.box!;
  expect(boxRef).toMatch(/^fake\//);
  await send.getByRole('button', { name: 'Open' }).click();
  await expect(send).toHaveCount(0);

  // The box pane runs in the box, with the uncommitted file.
  const boxPane = sent!.result!.pane!;
  host.cli(['pane', 'send-text', boxPane, 'cat wip.txt; echo BOX-$((40+2))']);
  host.cli(['pane', 'send-keys', boxPane, 'enter']);
  await host.until(() => host.cli(['pane', 'read', boxPane]).includes('BOX-42'), 30_000, 'box shell did not answer');
  expect(host.cli(['pane', 'read', boxPane])).toContain('uncommitted work');

  // The Sandboxes screen lists the box: running and yours.
  await palette(page, 'Sandboxes', 'Sandboxes');
  const box = page.locator(`[data-box="${boxRef}"]`);
  await expect(box).toBeVisible({ timeout: 20_000 });
  await expect(box).toContainText(boxRef.split('/')[1]!);
  await expect(box.getByText('running', { exact: true })).toBeVisible();
  await expect(box.getByText('Yours', { exact: true })).toBeVisible();
  await expect(page.getByText(/^1 running · 0 idle$/)).toBeVisible();
  await shoot(app, page, 'cloud-sandboxes-running');

  // Bring it back from the box's pane with the palette: to this host.
  await box.getByRole('button', { name: 'Open' }).click();
  await expect(page).toHaveURL(/#\/(w|p)\//);
  // A pane in a box: the header button brings it back instead.
  await page.getByRole('banner').getByRole('button', { name: 'Bring back', exact: true }).click();
  const back = page.getByRole('dialog', { name: 'Bring back from cloud' });
  await expect(back).toBeVisible();
  await back.getByRole('button', { name: /This host/ }).click();
  await expect(back.getByText('Done.', { exact: true })).toBeVisible({ timeout: 120_000 });
  await shoot(app, page, 'cloud-bring-back-done');
  const brought = jobs().find((j) => j.direction === 'bring_back' && j.state === 'done');
  expect(brought?.result?.pane).toBeTruthy();
  if (brought?.result?.worktree) expect(existsSync(join(brought.result.worktree, 'wip.txt'))).toBe(true);
  await back.getByRole('button', { name: 'Open' }).click();
  await expect(back).toHaveCount(0);
  const hostPaneHash = await page.evaluate(() => location.hash);

  // The box sleeps, with nothing left that is not on the host.
  await palette(page, 'Sandboxes', 'Sandboxes');
  await expect(box.getByText('paused', { exact: true })).toBeVisible({ timeout: 20_000 });
  await expect(box.getByText('Not synced')).toHaveCount(0);
  await expect(page.getByText(/^0 running · 1 idle$/)).toBeVisible();
  await shoot(app, page, 'cloud-sandboxes-paused');

  // needs_auth: the sign-in goes away while the send sheet is open → the sign-in prompt opens
  // → after signing in, the same send goes on.
  await page.evaluate((h) => (location.hash = h), hostPaneHash);
  await palette(page, 'Send to cloud', 'Send to cloud…');
  await expect(send).toBeVisible();
  await send.getByRole('button', { name: new RegExp(FAKE.replace(/[()]/g, '\\$&')) }).click();
  await send.getByRole('button', { name: /A new sandbox/ }).click();
  api('cloud.auth.clear', { provider: 'fake' });
  await send.getByRole('button', { name: `Send to ${FAKE}` }).click();
  await expect(signIn).toBeVisible({ timeout: 20_000 });
  await shoot(app, page, 'cloud-needs-auth');
  await signIn.getByLabel('Fake token').fill('fake-token');
  await signIn.getByRole('button', { name: 'Save' }).click();
  await expect(signIn).toHaveCount(0);
  await expect(send.getByText('Done.', { exact: true })).toBeVisible({ timeout: 120_000 });
  expect(jobs().filter((j) => j.direction === 'send' && j.state === 'done')).toHaveLength(2);
  await send.getByRole('button', { name: 'Done', exact: true }).click();
});
