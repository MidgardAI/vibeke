// Right panel against a real server + gateway: the Changes tree (compressed folders, rolled-up
// counts, expand/collapse, keyboard), the inline diff, the Commits section and a commit's files,
// the Files tab (lazy folders, file viewer), and the phone-size full-screen panel with an inline
// diff. Captures light/dark screenshots at 1440×900 and 390×844.

import { execFileSync } from 'node:child_process';
import { mkdirSync, rmSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { TestHost, agentHooks, built, hasDisplay, launchApp, shoot, vibekeBin, type LaunchedApp } from './helpers';

const bin = vibekeBin();
test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'run `bun run build` first');
test.skip(!bin, 'build the CLI first: cargo build -p vibeke (or set VIBEKE_BIN)');

let host: TestHost;
let a: LaunchedApp | null = null;

test.beforeAll(async () => {
  host = new TestHost(bin!);
  await host.start();
});

test.afterAll(async () => {
  await a?.close();
  await host?.stop();
});

const write = (cwd: string, files: Record<string, string>) => {
  for (const [f, body] of Object.entries(files)) {
    mkdirSync(join(cwd, f, '..'), { recursive: true });
    writeFileSync(join(cwd, f), body);
  }
};

const lines = (n: number, f: (i: number) => string) => Array.from({ length: n }, (_, i) => f(i)).join('\n') + '\n';

/** A repo with three commits and uncommitted edits spread over nested folders. */
function repo(name: string): { pane: string; cwd: string; git: (...args: string[]) => string } {
  const cwd = join(host.root, name);
  mkdirSync(cwd, { recursive: true });
  const git = (...args: string[]) => execFileSync('git', ['-c', 'user.email=e2e@example.com', '-c', 'user.name=e2e', ...args], { cwd, env: host.env, encoding: 'utf8' });
  git('init', '-q', '-b', 'feat/homepage-hero');
  write(cwd, {
    'README.md': `# ${name}\n\nThe website.\n`,
    'package.json': '{\n  "name": "website",\n  "private": true\n}\n',
    'src/app.ts': 'export const answer = 41;\n',
    'src/components/mockup/chat.tsx': lines(30, (i) => `export const line${i} = ${i};`),
    'src/components/mockup/legacy-tabs.tsx': lines(12, (i) => `// legacy ${i}`),
    'src/styles/mockup.css': '.mockup {\n  color: red;\n}\n',
  });
  git('add', '.');
  git('commit', '-q', '-m', 'Scaffold the website');
  write(cwd, { 'src/routes/index.tsx': 'export default function Index() {\n  return null;\n}\n' });
  git('add', '.');
  git('commit', '-q', '-m', 'Add the index route');
  write(cwd, { 'src/routes/index.tsx': 'export default function Index() {\n  return <main>Agents, in one place</main>;\n}\n', 'docs/plan.md': '# Plan\n' });
  git('add', '.');
  git('commit', '-q', '-m', 'Render the hero on the index route');
  git('mv', 'docs/plan.md', 'docs/roadmap.md');
  git('commit', '-q', '-m', 'Rename the plan to the roadmap');
  // The branch tracks a local `main` at the root commit (a base to compare with).
  git('branch', 'main', git('rev-list', '--max-parents=0', 'HEAD').trim());
  git('branch', '--set-upstream-to=main');
  // Uncommitted work.
  write(cwd, {
    'src/app.ts': 'export const answer = 42;\nexport const hero = "Agents, in one place";\n',
    'src/components/mockup/chat.tsx': lines(34, (i) => (i % 7 === 3 ? `export const line${i} = ${i * 2}; // changed` : `export const line${i} = ${i};`)),
    'src/components/mockup/atoms.tsx': lines(18, (i) => `export const atom${i} = '${i}';`),
    'src/components/desktop/window.tsx': 'export function Window() {\n  return <div className="window" />;\n}\n',
    'src/styles/mockup.css': '.mockup {\n  color: var(--fg);\n  display: grid;\n}\n',
    '.gitignore': 'dist/\n',
    'dist/bundle.js': 'console.log(1);\n',
  });
  rmSync(join(cwd, 'src/components/mockup/legacy-tabs.tsx'));
  const r = JSON.parse(host.cli(['workspace', 'create', '--cwd', cwd, '--name', name]));
  return { pane: r.root_pane.handle as string, cwd, git };
}

async function resize(page: Page, width: number, height: number) {
  await a!.app.evaluate(({ BrowserWindow }, [w, h]) => {
    const win = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL().includes('surface=full'));
    win?.setContentSize(w!, h!);
  }, [width, height]);
  await page.waitForFunction(([w]) => window.innerWidth === w, [width], { timeout: 5000 }).catch(() => page.setViewportSize({ width, height }));
}

test('right panel: changes tree, diff, commits, files, phone layout', async () => {
  test.setTimeout(180_000);
  const site = repo('website');
  await agentHooks(host, site.pane, site.cwd, [
    { event: 'SessionStart', extra: { source: 'startup' } },
    { event: 'UserPromptSubmit', extra: { prompt: 'Rebuild the homepage hero' } },
  ]);

  a = await launchApp({ ...host.env, VIBEKE_BIN: bin!, HOME: process.env.HOME ?? host.env.HOME! });
  const { page, app } = a;
  await resize(page, 1440, 900);
  await page.getByRole('button', { name: /Connect to this (Mac|computer)/ }).click();
  await expect(page.getByText(/^Connected to /)).toBeVisible({ timeout: 30_000 });
  await page.getByRole('button', { name: 'Open Vibeke' }).click();
  await page.getByRole('button', { name: 'Skip' }).click();

  const sidebar = page.getByRole('navigation', { name: 'Workspaces' });
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'website' }).click();
  const panel = page.getByRole('complementary', { name: 'Workspace panel' });
  await expect(panel).toBeVisible();
  const tree = panel.getByRole('tree', { name: 'Changed files' });
  const item = (name: string | RegExp) => tree.getByRole('treeitem', { name });

  // Single-child chains compress (`src` holds files and folders, so it stays; `components` too).
  await expect(item(/^atoms\.tsx/)).toBeVisible({ timeout: 20_000 });
  await expect(item(/^src/)).toHaveAttribute('aria-expanded', 'true');
  await expect(item(/^components/)).toBeVisible();
  await expect(item(/^legacy-tabs\.tsx/)).toBeVisible();
  await expect(tree.getByRole('treeitem')).toHaveCount(12);
  // Untracked text files carry line counts (dels 0), and the header total = the sum of the files.
  await expect(item(/^atoms\.tsx/)).toContainText('+18');
  await expect(item(/^atoms\.tsx/).getByRole('img', { name: 'Untracked' })).toBeVisible();
  await expect(item(/^window\.tsx/)).toContainText('+3');
  const tracked = site
    .git('diff', '--numstat', 'HEAD')
    .trim()
    .split('\n')
    .map((l) => l.split('\t').map(Number))
    .reduce((t, [a, d]) => [t[0]! + a!, t[1]! + d!], [0, 0]);
  const untracked = 18 + 3 + 1; // atoms.tsx, window.tsx, .gitignore
  await expect(panel.getByTestId('changes-total')).toHaveText(`+${tracked[0]! + untracked}−${tracked[1]}`);
  await expect(panel.getByRole('button', { name: 'Compare' })).toContainText('Uncommitted');
  await expect(panel.getByRole('button', { name: 'Branches' }).or(panel.getByText('feat/homepage-hero'))).toBeVisible();

  // Collapse and expand a folder by click; it is remembered.
  await item(/^mockup(?!\.)/).click();
  await expect(item(/^atoms\.tsx/)).toHaveCount(0);
  await expect(item(/^mockup(?!\.)/)).toHaveAttribute('aria-expanded', 'false');
  await item(/^mockup(?!\.)/).click();
  await expect(item(/^atoms\.tsx/)).toBeVisible();

  // Keyboard: j/k move, ← collapses, → expands, Enter opens.
  await item(/^components/).focus();
  await page.keyboard.press('j');
  await expect(item(/^desktop/)).toBeFocused();
  await page.keyboard.press('ArrowLeft');
  await expect(item(/^window\.tsx/)).toHaveCount(0);
  await page.keyboard.press('ArrowRight');
  await expect(item(/^window\.tsx/)).toBeVisible();
  await page.keyboard.press('k');
  await expect(item(/^components/)).toBeFocused();
  await shoot(app, page, 'panel-tree');

  // ⌥-click opens the diff in the centre as a transient view; the tree stays.
  await item(/^mockup\.css/).click({ modifiers: ['Alt'] });
  await expect(page).toHaveURL(/view=diff/);
  const closeDiff = page.getByRole('button', { name: 'Close diff' });
  await expect(closeDiff).toBeVisible({ timeout: 15_000 });
  await expect(page.locator('[data-diff-scroll]').filter({ hasText: 'display: grid;' })).toBeVisible({ timeout: 15_000 });
  await expect(tree).toBeVisible();
  await closeDiff.click();
  await expect(page).not.toHaveURL(/view=diff/);

  // Open a diff inline: sticky header, highlighted lines, prev/next.
  await item(/^app\.ts/).click();
  await expect(page).toHaveURL(/file=src%2Fapp\.ts/);
  await expect(panel.locator('[data-diff-scroll]')).toContainText('export const answer = 42;', { timeout: 15_000 });
  await expect(panel.locator('[data-diff-scroll] .tk-kw').first()).toBeVisible();
  await shoot(app, page, 'panel-diff');
  // An edit that keeps the line counts still reaches the open diff (next status poll).
  write(site.cwd, { 'src/app.ts': 'export const answer = 43;\nexport const hero = "Agents, in one place";\n' });
  await expect(panel.locator('[data-diff-scroll]')).toContainText('export const answer = 43;', { timeout: 12_000 });
  await panel.getByRole('button', { name: 'Next file' }).click();
  await expect(page).not.toHaveURL(/file=src%2Fapp\.ts/);
  await panel.getByRole('button', { name: 'Back', exact: true }).click();
  await expect(tree).toBeVisible();

  // Commits: the section lists git.log; a commit shows its files and their diffs.
  const commits = panel.getByRole('region', { name: 'Commits' });
  await commits.getByRole('button', { name: /Commits/ }).click();
  await expect(commits).toContainText('Render the hero on the index route', { timeout: 15_000 });
  await expect(commits).toContainText('Scaffold the website');
  await commits.getByRole('listitem').filter({ hasText: 'Render the hero' }).click();
  await expect(page).toHaveURL(/commit=[0-9a-f]{7,}/);
  await expect(item(/^index\.tsx/)).toBeVisible({ timeout: 15_000 });
  await expect(item(/^plan\.md/)).toBeVisible();
  await expect(tree.getByRole('treeitem', { name: /^app\.ts/ })).toHaveCount(0);
  // Status squares on a commit's files, like the uncommitted tree.
  await expect(item(/^index\.tsx/).getByRole('img', { name: 'Modified' })).toBeVisible();
  await expect(item(/^plan\.md/).getByRole('img', { name: 'Added' })).toBeVisible();
  await page.mouse.move(5, 450);
  await shoot(app, page, 'panel-commits');
  await item(/^index\.tsx/).click();
  await expect(panel.locator('[data-diff-scroll]')).toContainText('Agents, in one place', { timeout: 15_000 });
  await panel.getByRole('button', { name: 'Back', exact: true }).click();
  // A rename shows its square and where it came from.
  await commits.getByRole('listitem').filter({ hasText: 'Rename the plan' }).click();
  await expect(item(/^roadmap\.md/)).toContainText('from docs/plan.md', { timeout: 15_000 });
  await expect(item(/^roadmap\.md/).getByRole('img', { name: 'Renamed' })).toBeVisible();
  // The root commit diffs against the empty tree — in the panel and in the centre view.
  await commits.getByRole('listitem').filter({ hasText: 'Scaffold the website' }).click();
  await expect(item(/^package\.json/)).toBeVisible({ timeout: 15_000 });
  await expect(item(/^package\.json/).getByRole('img', { name: 'Added' })).toBeVisible();
  await item(/^package\.json/).click({ modifiers: ['Alt'] });
  await expect(page).toHaveURL(/view=diff/);
  await expect(page.locator('[data-diff-scroll]').filter({ hasText: '"private": true' })).toBeVisible({ timeout: 15_000 });
  await page.getByRole('button', { name: 'Close diff' }).click();
  // A direct route to a root commit's file in the centre (fresh renderer, nothing cached).
  const rootSha = site.git('rev-list', '--max-parents=0', 'HEAD').trim();
  await page.evaluate((sha) => {
    const path = location.hash.split('?')[0];
    location.hash = `${path}?commit=${sha}&file=README.md&view=diff`;
    location.reload();
  }, rootSha);
  await expect(page.locator('[data-diff-scroll]').filter({ hasText: 'The website.' })).toBeVisible({ timeout: 20_000 });
  await page.getByRole('button', { name: 'Close diff' }).click();
  await expect(item(/^package\.json/)).toBeVisible({ timeout: 15_000 });
  await panel.getByRole('button', { name: 'Back to changes' }).click();
  await expect(page).not.toHaveURL(/commit=/);
  await expect(item(/^atoms\.tsx/)).toBeVisible();

  // vs base: the listing compares with `main` (incl. the working tree); a manual refresh brings an
  // edit that keeps the counts into the open diff.
  await panel.getByRole('button', { name: 'Compare' }).click();
  await page.getByRole('menuitem', { name: 'vs main' }).click();
  await expect(page).toHaveURL(/base=main/);
  await expect(item(/^roadmap\.md/).getByRole('img', { name: 'Added' })).toBeVisible({ timeout: 15_000 });
  await item(/^app\.ts/).click();
  await expect(panel.locator('[data-diff-scroll]')).toContainText('export const answer = 43;', { timeout: 15_000 });
  write(site.cwd, { 'src/app.ts': 'export const answer = 44;\nexport const hero = "Agents, in one place";\n' });
  await panel.getByRole('button', { name: 'Refresh' }).click();
  await expect(panel.locator('[data-diff-scroll]')).toContainText('export const answer = 44;', { timeout: 3_000 });
  await panel.getByRole('button', { name: 'Back', exact: true }).click();
  await panel.getByRole('button', { name: 'Compare' }).click();
  await page.getByRole('menuitem', { name: 'Uncommitted' }).click();
  await expect(page).not.toHaveURL(/base=/);
  await expect(item(/^atoms\.tsx/)).toBeVisible();

  // Files: lazy folders, ignored entries dimmed, the read-only viewer.
  await panel.getByRole('tab', { name: 'Files' }).click();
  const files = panel.getByRole('tree', { name: 'Files' });
  await expect(files.getByRole('treeitem', { name: /^src/ })).toBeVisible({ timeout: 15_000 });
  await expect(files.getByRole('treeitem', { name: /^dist/ })).toHaveClass(/opacity/);
  await files.getByRole('treeitem', { name: /^src/ }).click();
  await files.getByRole('treeitem', { name: /^app\.ts/ }).click();
  await expect(panel).toContainText('export const hero', { timeout: 15_000 });
  await shoot(app, page, 'panel-files');
  await panel.getByRole('button', { name: 'Back', exact: true }).click();
  await panel.getByRole('tab', { name: 'Changes' }).click();
  await expect(item(/^atoms\.tsx/)).toBeVisible({ timeout: 15_000 });

  // Phone: the panel covers the screen; a file opens an inline diff.
  await resize(page, 390, 844);
  await expect(panel).toHaveCount(0);
  await page.getByRole('button', { name: /Toggle changes panel/ }).first().click();
  const sheet = page.getByRole('dialog', { name: 'Workspace panel' });
  await expect(sheet).toBeVisible();
  await expect(sheet.getByRole('treeitem', { name: /^atoms\.tsx/ })).toBeVisible({ timeout: 20_000 });
  await shoot(app, page, 'panel-mobile-changes');
  await sheet.getByRole('treeitem', { name: /^chat\.tsx/ }).click();
  await expect(sheet.locator('[data-diff-scroll]')).toContainText('// changed', { timeout: 15_000 });
  await shoot(app, page, 'panel-mobile-diff');
  await page.keyboard.press('Escape');
  await expect(sheet).toHaveCount(0);
  await resize(page, 1440, 900);
});
