// Workspace-first shell against a real server + gateway: workspaces in status groups in the
// sidebar (an agent working, one waiting for an approval, one finished, an idle shell), opening a
// workspace from the sidebar, the changes panel (⌘3 / button), the inbox, light/dark captures at
// desktop size, and the narrow layout (drawer sidebar, full-screen panel) at phone size.

import { execFileSync } from 'node:child_process';
import { mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { TestHost, agentHooks, built, hasDisplay, launchApp, settled, shoot, vibekeBin, type LaunchedApp } from './helpers';

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

/** A git repo on `branch` with one commit and some uncommitted edits, as a workspace. */
function repoWorkspace(name: string, branch: string, edits: Record<string, string> = {}): { pane: string; cwd: string } {
  const cwd = join(host.root, name);
  mkdirSync(join(cwd, 'src'), { recursive: true });
  const git = (...args: string[]) => execFileSync('git', ['-c', 'user.email=e2e@example.com', '-c', 'user.name=e2e', ...args], { cwd, env: host.env, stdio: 'ignore' });
  git('init', '-q', '-b', branch);
  writeFileSync(join(cwd, 'README.md'), `# ${name}\n`);
  writeFileSync(join(cwd, 'src/app.ts'), 'export const answer = 41;\n');
  git('add', '.');
  git('commit', '-q', '-m', 'init');
  for (const [f, body] of Object.entries(edits)) {
    mkdirSync(join(cwd, f, '..'), { recursive: true });
    writeFileSync(join(cwd, f), body);
  }
  const r = JSON.parse(host.cli(['workspace', 'create', '--cwd', cwd, '--name', name]));
  return { pane: r.root_pane.handle as string, cwd };
}

async function resize(page: Page, width: number, height: number) {
  await a!.app.evaluate(({ BrowserWindow }, [w, h]) => {
    const win = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL().includes('surface=full'));
    win?.setContentSize(w!, h!);
  }, [width, height]);
  await page.waitForFunction(([w]) => window.innerWidth === w, [width], { timeout: 5000 }).catch(() => page.setViewportSize({ width, height }));
}

test('workspace sidebar, panel and narrow layout', async () => {
  test.setTimeout(180_000);
  const homepage = repoWorkspace('homepage', 'feat/homepage-hero', {
    'src/app.ts': 'export const answer = 42;\nexport const hero = "Agents, in one place";\n',
    'src/hero.tsx': 'export function Hero() {\n  return <h1>Hello</h1>;\n}\n',
    'docs/notes.md': '# Notes\n',
  });
  const auth = repoWorkspace('api-auth', 'fix/token-refresh', { 'src/app.ts': 'export const answer = 43;\n' });
  const release = repoWorkspace('release-notes', 'main');
  host.workspace('scratch');

  // Agents: one working, one finished, one waiting for an approval.
  await agentHooks(host, homepage.pane, homepage.cwd, [
    { event: 'SessionStart', extra: { source: 'startup' } },
    { event: 'UserPromptSubmit', extra: { prompt: 'Rebuild the homepage hero' } },
  ]);
  await agentHooks(host, release.pane, release.cwd, [
    { event: 'SessionStart', extra: { source: 'startup' } },
    { event: 'UserPromptSubmit', extra: { prompt: 'Draft the release notes' } },
    { event: 'Stop' },
  ]);
  host.requestApproval(auth.pane, 'cargo test -p auth', auth.cwd);

  a = await launchApp({ ...host.env, VIBEKE_BIN: bin!, HOME: process.env.HOME ?? host.env.HOME! });
  const { page, app } = a;
  await resize(page, 1440, 900);
  await page.getByRole('button', { name: /Connect to this (Mac|computer)/ }).click();
  await expect(page.getByText(/^Connected to /)).toBeVisible({ timeout: 30_000 });
  await page.getByRole('button', { name: 'Open Vibeke' }).click();
  await page.getByRole('button', { name: 'Skip' }).click();

  const sidebar = page.getByRole('navigation', { name: 'Workspaces' });
  const rows = sidebar.locator('[data-nav-item]');
  await expect(rows).toHaveCount(4, { timeout: 30_000 });
  await expect(sidebar.getByRole('region', { name: 'Needs you' })).toContainText('api-auth', { timeout: 30_000 });
  await expect(sidebar.getByRole('region', { name: 'Idle' })).toContainText('scratch');

  // Inbox: the approval card, the badge in the sidebar.
  await page.keyboard.press(`${mod}+1`);
  await expect(page).toHaveURL(/#\/inbox$/);
  await expect(page.locator('main [data-nav-item]').filter({ hasText: 'cargo test -p auth' })).toBeVisible({ timeout: 30_000 });
  await shoot(app, page, 'shell-inbox');

  // Open a workspace from the sidebar: its pane in the centre, the changes panel docked.
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'homepage' }).click();
  await expect(page).toHaveURL(/#\/w\/[^/]+\/[^/?]+/);
  const panel = page.getByRole('complementary', { name: 'Workspace panel' });
  await expect(panel).toBeVisible();
  await expect(panel).toContainText('hero.tsx', { timeout: 20_000 });
  await shoot(app, page, 'shell-workspace');

  // ⌘3 hides and shows the panel; the choice sticks for the next workspace.
  await page.locator('body').click({ position: { x: 700, y: 400 } }).catch(() => {});
  await page.keyboard.press(`${mod}+3`);
  await expect(panel).toHaveCount(0);
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'api-auth' }).click();
  await expect(panel).toHaveCount(0);
  await page.getByRole('button', { name: /Toggle changes panel/ }).first().click();
  await expect(panel).toBeVisible();

  // j/k walk the sidebar when the centre has no list; Enter opens.
  await page.keyboard.press(`${mod}+2`);
  await expect(page).toHaveURL(/#\/w\//);

  // Narrow: the sidebar becomes a drawer, the panel a full-screen layer.
  await resize(page, 390, 844);
  await expect(sidebar).toHaveCount(0);
  await page.getByRole('button', { name: /Open sidebar/ }).first().click();
  const drawer = page.getByRole('dialog', { name: 'Workspaces' });
  await expect(drawer).toBeVisible();
  await settled(page);
  await shoot(app, page, 'shell-mobile-drawer');
  await drawer.locator('[data-nav-item]').filter({ hasText: 'homepage' }).click();
  await expect(drawer).toHaveCount(0);
  await shoot(app, page, 'shell-mobile-workspace');
  await page.getByRole('button', { name: /Toggle changes panel/ }).first().click();
  const sheet = page.getByRole('dialog', { name: 'Workspace panel' });
  await expect(sheet).toBeVisible();
  await expect(sheet).toContainText('hero.tsx', { timeout: 20_000 });
  await shoot(app, page, 'shell-mobile-panel');
  await page.keyboard.press('Escape');
  await expect(sheet).toHaveCount(0);
  await resize(page, 1440, 900);
});
