// Folder confirmation and worktree selection use the real shared React components.
import { execFileSync } from 'node:child_process';
import { mkdtempSync, readFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { expect, test } from '@playwright/test';
import { appRoot, built, hasDisplay, launchApp, type LaunchedApp } from './helpers';
import type {} from './fixtures/new-sheet';

test.skip(!hasDisplay(), 'no display');
test.skip(!built(), 'run `bun run build` first');
let dir: string;
let script: string;
let a: LaunchedApp;

test.beforeAll(() => {
  dir = mkdtempSync(join(tmpdir(), 'vibeke-new-sheet-'));
  const bundle = join(dir, 'new-sheet.js');
  execFileSync('bun', ['build', 'e2e/fixtures/new-sheet.tsx', '--target=browser', '--format=iife', `--outfile=${bundle}`], { cwd: appRoot });
  script = readFileSync(bundle, 'utf8');
});
test.beforeEach(async () => {
  a = await launchApp();
  await a.page.evaluate(script);
  await expect(a.page.getByRole('dialog', { name: 'New', exact: true })).toBeVisible();
});
test.afterEach(async () => { await a?.close(); });
test.afterAll(() => { rmSync(dir, { recursive: true, force: true }); });

test('browsing a folder can be confirmed and starts a workspace there', async () => {
  const { page } = a;
  await expect(page.getByRole('switch', { name: 'New worktree' })).toBeEnabled();
  await page.getByRole('button', { name: 'New folder…', exact: true }).click();
  const path = page.getByRole('combobox', { name: 'Choose a folder on the host' });
  await path.fill('~/code/');
  await page.getByRole('option', { name: /^repo\b/ }).click();
  await expect(path).toHaveValue('~/code/repo/');
  await expect(page.getByRole('button', { name: 'Start', exact: true })).toBeDisabled();
  await page.screenshot({ path: join(appRoot, 'test-results/new-sheet-folder.png') });
  await page.getByRole('button', { name: 'Use this folder' }).click();
  await expect(path).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'repo', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByRole('switch', { name: 'New worktree' })).toBeEnabled();
  await page.getByRole('button', { name: 'Start', exact: true }).click();
  const calls = await page.evaluate(() => window.newSheetTest.calls);
  expect(calls.find((c) => c.method === 'agent.start')?.params).toMatchObject({ new_workspace: { cwd: '/host/code/repo' } });
  expect(calls.find((c) => c.method === 'agent.start')?.params.worktree).toBeUndefined();
});

test('a linked favorite uses its canonical cwd, current branch, and Again repeats the worktree', async () => {
  const { page } = a;
  await page.getByRole('button', { name: 'linked', exact: true }).click();
  const toggle = page.getByRole('switch', { name: 'New worktree' });
  await expect(toggle).toBeEnabled();
  await toggle.click();
  await page.getByRole('button', { name: 'Current branch (feature)' }).click();
  await page.getByRole('textbox', { name: 'Branch name' }).fill('agent/folder-fix');
  await page.getByRole('button', { name: 'Start', exact: true }).click();
  await expect(page.getByRole('dialog', { name: 'New', exact: true })).toHaveCount(0);
  let starts = await page.evaluate(() => window.newSheetTest.calls.filter((c) => c.method === 'agent.start'));
  expect(starts[0]!.params).toMatchObject({ cwd: '/host/code/linked', worktree: { branch: 'agent/folder-fix', base: 'feature' } });
  expect(starts[0]!.params.new_workspace).toBeUndefined();
  expect(starts[0]!.params.workspace).toBeUndefined();
  await page.evaluate(() => window.newSheetTest.reopen());
  await page.getByRole('button', { name: /^Again:/ }).click();
  await expect(toggle).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByRole('textbox', { name: 'Branch name' })).not.toHaveValue('');
  await page.getByRole('button', { name: 'Start', exact: true }).click();
  starts = await page.evaluate(() => window.newSheetTest.calls.filter((c) => c.method === 'agent.start'));
  expect(starts).toHaveLength(2);
  expect(starts[1]!.params).toMatchObject({ cwd: '/host/code/linked', worktree: { base: 'main' } });
});

test('Enter confirms a repository subfolder even when it has no .git entry', async () => {
  const { page } = a;
  await page.getByRole('button', { name: 'New folder…', exact: true }).click();
  const path = page.getByRole('combobox', { name: 'Choose a folder on the host' });
  await path.fill('~/code/repo/docs');
  await path.press('Enter');
  await expect(path).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'docs', exact: true })).toHaveAttribute('aria-pressed', 'true');
  await expect(page.getByRole('switch', { name: 'New worktree' })).toBeEnabled();
});

test('empty and missing paths cannot be confirmed; non-Git folders still allow ordinary starts', async () => {
  const { page } = a;
  await page.getByRole('button', { name: 'New folder…', exact: true }).click();
  const path = page.getByRole('combobox', { name: 'Choose a folder on the host' });
  await path.fill('');
  await expect(page.getByRole('button', { name: 'Use this folder' })).toBeDisabled();
  await path.fill('~/code/missing');
  await page.getByRole('button', { name: 'Use this folder' }).click();
  await expect(page.getByRole('alert')).toContainText('no such folder');
  await expect(path).toBeVisible();
  await path.fill('~/code/scratch');
  await path.press('Enter');
  await expect(path).toHaveCount(0);
  await expect(page.getByText('This folder is not in a Git repository.')).toBeVisible();
  await expect(page.getByRole('switch', { name: 'New worktree' })).toBeDisabled();
  await expect(page.getByRole('button', { name: 'Start', exact: true })).toBeEnabled();
});

test('stale Git replies cannot enable worktrees for a different folder', async () => {
  const { page } = a;
  await page.evaluate(() => window.newSheetTest.hold('worktree.list', '/host/code/linked'));
  await page.getByRole('button', { name: 'linked', exact: true }).click();
  await page.waitForFunction(() => window.newSheetTest.calls.some((c) => c.method === 'worktree.list' && c.params.cwd === '/host/code/linked'));
  await page.getByRole('button', { name: 'scratch', exact: true }).click();
  await expect(page.getByText('This folder is not in a Git repository.')).toBeVisible();
  await page.evaluate(() => window.newSheetTest.release('worktree.list', '/host/code/linked'));
  await expect(page.getByRole('switch', { name: 'New worktree' })).toBeDisabled();
  await expect(page.getByText('This folder is not in a Git repository.')).toBeVisible();
});

test('a delayed confirmation cannot replace a folder edited while it was pending', async () => {
  const { page } = a;
  await page.getByRole('button', { name: 'New folder…', exact: true }).click();
  const path = page.getByRole('combobox', { name: 'Choose a folder on the host' });
  await path.fill('~/code/repo');
  await page.evaluate(() => window.newSheetTest.hold('fs.browse', '/host/code/repo'));
  await page.getByRole('button', { name: 'Use this folder' }).click();
  await expect(page.getByRole('button', { name: 'Use this folder' })).toBeDisabled();
  await path.fill('~/code/scratch/');
  await page.evaluate(() => window.newSheetTest.release('fs.browse', '/host/code/repo'));
  await expect(path).toHaveValue('~/code/scratch/');
  await expect(page.getByRole('button', { name: 'Use this folder' })).toBeEnabled();
});

test('Git errors stay visible and limited devices cannot choose folders or worktrees', async () => {
  const { page } = a;
  await page.getByRole('button', { name: 'New folder…', exact: true }).click();
  await page.getByRole('combobox', { name: 'Choose a folder on the host' }).fill('~/code/denied');
  await page.getByRole('button', { name: 'Use this folder' }).click();
  await expect(page.getByText('Permission denied while checking Git')).toBeVisible();
  await expect(page.getByText('This folder is not in a Git repository.')).toHaveCount(0);
  await page.evaluate(() => window.newSheetTest.restrict());
  await expect(page.getByRole('button', { name: 'New folder…', exact: true })).toBeDisabled();
  await expect(page.getByRole('switch', { name: 'New worktree' })).toBeDisabled();
});
