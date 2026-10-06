// Conversation-first workspace centre against a real server + gateway: a Claude-style agent whose
// SessionStart hook names a transcript file, so `agent.transcript` serves real turns. Checks the
// user pill, tool rows with summaries, the folded "N steps", the turn footer ("Worked for …",
// counts), the composer labels, the Terminal tab, the + menu, and an approval rendered inline at
// the end of another agent's stream (`data-act` kept). Light/dark captures at 1440×900 and
// 390×844.

import { execFileSync } from 'node:child_process';
import { appendFileSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { TestHost, built, hasDisplay, hookEvent, launchApp, settled, shoot, vibekeBin, type LaunchedApp } from './helpers';

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

function repoWorkspace(name: string): { pane: string; cwd: string } {
  const cwd = join(host.root, name);
  mkdirSync(join(cwd, 'src'), { recursive: true });
  const git = (...args: string[]) => execFileSync('git', ['-c', 'user.email=e2e@example.com', '-c', 'user.name=e2e', ...args], { cwd, env: host.env, stdio: 'ignore' });
  git('init', '-q', '-b', 'feat/hero');
  writeFileSync(join(cwd, 'src/hero.tsx'), 'export function Hero() {\n  return <h1>Hello</h1>;\n}\n');
  git('add', '.');
  git('commit', '-q', '-m', 'init');
  writeFileSync(join(cwd, 'src/hero.tsx'), 'export function Hero() {\n  return <h1>Agents, in one place</h1>;\n}\nexport const tagline = "Ship faster";\n');
  // A new, untracked file: the Changes tree counts its lines too.
  writeFileSync(join(cwd, 'src/hero-copy.ts'), ['export const copy = {', '  title: "Agents, in one place",', '  tagline: "Ship faster",', '  cta: "Get started",', '};', ''].join('\n'));
  const r = JSON.parse(host.cli(['workspace', 'create', '--cwd', cwd, '--name', name]));
  return { pane: r.root_pane.handle as string, cwd };
}

/** A Claude-format JSONL transcript: a short first turn, then a long one with many tool calls. */
function writeTranscript(path: string, cwd: string): void {
  const at = (min: number, sec: number) => new Date(Date.UTC(2026, 9, 6, 12, min, sec)).toISOString();
  const lines: unknown[] = [];
  const user = (ts: string, text: string) => lines.push({ type: 'user', timestamp: ts, message: { role: 'user', content: text } });
  const say = (ts: string, text: string) => lines.push({ type: 'assistant', timestamp: ts, message: { role: 'assistant', content: [{ type: 'text', text }] } });
  let id = 0;
  const tool = (ts: string, name: string, input: Record<string, unknown>, out: string, error = false) => {
    const tid = `toolu_${++id}`;
    lines.push({ type: 'assistant', timestamp: ts, message: { role: 'assistant', content: [{ type: 'tool_use', id: tid, name, input }] } });
    lines.push({ type: 'user', timestamp: ts, message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: tid, content: out, is_error: error }] } });
  };
  user(at(0, 0), 'What does this repo contain?');
  tool(at(0, 3), 'Bash', { command: 'ls -la src', description: 'List files' }, 'hero.tsx');
  say(at(0, 9), 'A single React component, `Hero`, in `src/hero.tsx`.');
  user(at(1, 0), 'Rebuild the homepage hero around the new tagline and make sure the tests pass.');
  say(at(1, 4), "I'll look at the current hero and the tests first.");
  tool(at(1, 6), 'Bash', { command: 'git status --short', description: 'Status' }, ' M src/hero.tsx');
  tool(at(1, 9), 'Read', { file_path: `${cwd}/src/hero.tsx` }, 'export function Hero() {…}');
  tool(at(2, 0), 'Grep', { pattern: 'tagline', path: `${cwd}/src` }, 'src/hero.tsx');
  tool(at(4, 0), 'Task', { description: 'Survey the landing page copy', prompt: 'Find all hero copy', subagent_type: 'Explore' }, 'Two places.');
  tool(at(9, 0), 'Edit', { file_path: `${cwd}/src/hero.tsx`, old_string: 'Hello', new_string: 'Agents, in one place' }, 'ok');
  tool(at(15, 0), 'Bash', { command: 'bun test src/hero.test.tsx', description: 'Run tests' }, '1 fail', true);
  tool(at(21, 0), 'Edit', { file_path: `${cwd}/src/hero.tsx`, old_string: 'x', new_string: 'y' }, 'ok');
  tool(at(28, 0), 'Bash', { command: 'bun test src/hero.test.tsx', description: 'Run tests' }, '3 pass');
  say(
    at(32, 46),
    'The hero is rebuilt around **“Agents, in one place”**.\n\n- Rewrote the heading and added the tagline export\n- Fixed the failing snapshot test\n\n```tsx\nexport const tagline = "Ship faster";\n```\n\nReady for review.',
  );
  writeFileSync(path, lines.map((l) => JSON.stringify(l)).join('\n') + '\n');
}

/** Wait until the pane's shell runs commands (it echoes a marker back). */
async function shellReady(pane: string): Promise<void> {
  const marker = `VK-READY-${Date.now()}`;
  host.cli(['pane', 'send-text', pane, `echo ${marker}`]);
  host.cli(['pane', 'send-keys', pane, 'enter']);
  await host.until(() => host.cli(['pane', 'read', pane]).split(marker).length > 2, 20_000, 'shell did not start');
}

async function resize(page: Page, width: number, height: number) {
  await a!.app.evaluate(({ BrowserWindow }, [w, h]) => {
    const win = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL().includes('surface=full'));
    win?.setContentSize(w!, h!);
  }, [width, height]);
  await page.waitForFunction(([w]) => window.innerWidth === w, [width], { timeout: 5000 }).catch(() => page.setViewportSize({ width, height }));
}

test('conversation: transcript turns, tool rows, footer, composer, terminal tab, inline approval', async () => {
  test.setTimeout(180_000);
  const hero = repoWorkspace('homepage');
  const transcript = join(host.root, 'hero-transcript.jsonl');
  writeTranscript(transcript, hero.cwd);
  // The hooks run in the pane's shell; keystrokes sent while the shell still starts can be lost,
  // so wait for it to echo, and send the hooks again if the run has not picked them up.
  await shellReady(hero.pane);
  const picked = () => {
    const runs = JSON.parse(host.cli(['agent', 'list'])).runs as { transcript_path: string | null; turns_completed: number }[];
    return runs.some((r) => r.transcript_path === transcript && r.turns_completed > 0);
  };
  for (let attempt = 0; attempt < 3 && !picked(); attempt++) {
    hookEvent(host, hero.pane, 'SessionStart', hero.cwd, { source: 'startup', transcript_path: transcript, model: 'opus', permission_mode: 'acceptEdits' });
    hookEvent(host, hero.pane, 'UserPromptSubmit', hero.cwd, { prompt: 'Rebuild the homepage hero' });
    hookEvent(host, hero.pane, 'Stop', hero.cwd);
    await host.until(picked, 15_000, 'hooks pending').catch(() => {});
  }
  expect(picked()).toBe(true);
  const auth = repoWorkspace('api-auth');
  host.requestApproval(auth.pane, 'cargo test -p auth', auth.cwd);

  a = await launchApp({ ...host.env, VIBEKE_BIN: bin!, HOME: process.env.HOME ?? host.env.HOME! });
  const { page, app } = a;
  await resize(page, 1440, 900);
  await page.getByRole('button', { name: /Connect to this (Mac|computer)/ }).click();
  await expect(page.getByText(/^Connected to /)).toBeVisible({ timeout: 30_000 });
  await page.getByRole('button', { name: 'Open Vibeke' }).click();
  await page.getByRole('button', { name: 'Skip' }).click();

  const sidebar = page.getByRole('navigation', { name: 'Workspaces' });
  await expect(sidebar.locator('[data-nav-item]')).toHaveCount(2, { timeout: 30_000 });
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'homepage' }).click();
  await expect(page).toHaveURL(/#\/w\/[^/]+\/[^/?]+/);

  // The agent tab is selected and shows its conversation.
  await expect(page.getByRole('tab', { selected: true })).not.toHaveText('Terminal');
  const log = page.getByRole('log', { name: 'Conversation' });
  await expect(log.locator('[data-role="user"]').filter({ hasText: 'Rebuild the homepage hero' })).toBeVisible({ timeout: 30_000 });
  await expect(log.getByText('Ready for review.')).toBeVisible();
  // Tool rows: label + mono summary, relative paths; more than five steps fold.
  const lastBash = log.locator('[data-tool="Bash"]').last();
  await expect(lastBash).toContainText('Shell');
  await expect(lastBash).toContainText('bun test src/hero.test.tsx');
  await expect(log.locator('[data-tool="Edit"]').last()).toContainText('src/hero.tsx');
  const folded = log.getByRole('button', { name: /^\d+ steps$/ });
  await expect(folded).toBeVisible();
  await folded.click();
  await expect(log.locator('[data-tool="Grep"]')).toContainText('tagline in src');
  await expect(log.locator('[data-tool="Task"]')).toContainText('Survey the landing page copy');
  // Tool details expand on click.
  await lastBash.getByRole('button').click();
  await expect(lastBash).toContainText('3 pass');
  // Footer of the latest turn: time worked, counts.
  await expect(log.getByText('Worked for 31m 46s')).toBeVisible();
  await expect(log.getByText('Worked for 9s · 1 tool')).toBeVisible();
  await expect(log.getByText('8 tools', { exact: true })).toBeVisible();
  await expect(log.getByText('1 subagent', { exact: true })).toBeVisible();
  // Composer: read-only harness and permission labels.
  await expect(page.getByRole('textbox', { name: 'Message the agent, tag @files, or use /commands' })).toBeVisible();
  await expect(page.getByText('Accept edits')).toBeVisible();
  // Collapse the details again for the capture.
  await lastBash.getByRole('button').first().click();
  await folded.click();
  // The panel's Changes tree lists the untracked file with its line count.
  const changed = page.getByRole('complementary', { name: 'Workspace panel' }).getByRole('tree', { name: 'Changed files' });
  if (await changed.count()) await expect(changed.getByRole('treeitem', { name: /^hero-copy\.ts/ })).toContainText('+5', { timeout: 15_000 });
  await page.mouse.move(5, 450);
  await settled(page);
  await shoot(app, page, 'conversation');

  // Live: a new turn written to the transcript shows up without a reload — the user's prompt and
  // a tool call while the agent works (turn_started / file_changed events), then the answer when
  // the turn completes. Events reach this window through main's forwarded host events.
  const append = (...lines: unknown[]) => appendFileSync(transcript, lines.map((l) => JSON.stringify(l)).join('\n') + '\n');
  const ts = (sec: number) => new Date(Date.UTC(2026, 9, 6, 12, 40, sec)).toISOString();
  // The CLI prints execution states as `Working` / `Idle`.
  const runState = () =>
    (JSON.parse(host.cli(['agent', 'list'])).runs as { transcript_path: string | null; execution: { value: string } }[]).find((r) => r.transcript_path === transcript)?.execution.value.toLowerCase();
  append({ type: 'user', timestamp: ts(0), message: { role: 'user', content: 'Now add a dark-mode variant of the hero.' } });
  hookEvent(host, hero.pane, 'UserPromptSubmit', hero.cwd, { prompt: 'Now add a dark-mode variant' });
  await host.until(() => runState() === 'working', 15_000, 'prompt hook not processed');
  await expect(log.locator('[data-role="user"]').filter({ hasText: 'dark-mode variant' })).toBeVisible({ timeout: 2_500 });
  // Working with nothing waiting on the user: the composer offers Stop.
  await expect(page.getByRole('button', { name: 'Interrupt' })).toBeVisible({ timeout: 2_500 });
  // Mid-turn: a tool call appears as it happens.
  append(
    { type: 'assistant', timestamp: ts(5), message: { role: 'assistant', content: [{ type: 'tool_use', id: 'toolu_live1', name: 'Edit', input: { file_path: `${hero.cwd}/src/hero-dark.tsx`, old_string: 'a', new_string: 'b' } }] } },
    { type: 'user', timestamp: ts(6), message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_live1', content: 'ok' }] } },
  );
  hookEvent(host, hero.pane, 'PostToolUse', hero.cwd, { tool_name: 'Edit', tool_use_id: 'toolu_live1', tool_input: { file_path: `${hero.cwd}/src/hero-dark.tsx` } });
  await expect(log.locator('[data-tool="Edit"]').filter({ hasText: 'hero-dark.tsx' })).toBeVisible({ timeout: 3_000 });
  append({ type: 'assistant', timestamp: ts(20), message: { role: 'assistant', content: [{ type: 'text', text: 'Added `HeroDark`, live without a reload.' }] } });
  hookEvent(host, hero.pane, 'Stop', hero.cwd);
  await host.until(() => runState() === 'idle', 15_000, 'stop hook not processed');
  await expect(log.getByText('live without a reload')).toBeVisible({ timeout: 2_500 });
  await expect(page.getByRole('button', { name: 'Interrupt' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: 'Send', exact: true })).toBeVisible();

  // Tabs keyboard: one tab stop, ←/→ move focus, Enter selects, Home/End jump; tab ↔ panel linked.
  const strip = page.getByRole('tablist', { name: 'Tabs' });
  const agentTab = strip.getByRole('tab', { selected: true });
  await expect(agentTab).toHaveAttribute('tabindex', '0');
  await expect(strip.getByRole('tab', { name: 'Terminal' })).toHaveAttribute('tabindex', '-1');
  const panelId = await agentTab.getAttribute('aria-controls');
  await expect(page.locator(`#${panelId}`)).toHaveAttribute('role', 'tabpanel');
  await expect(page.locator(`#${panelId}`)).toHaveAttribute('aria-labelledby', (await agentTab.getAttribute('id'))!);
  await agentTab.focus();
  await page.keyboard.press('ArrowRight');
  await expect(strip.getByRole('tab', { name: 'Terminal' })).toBeFocused();
  await page.keyboard.press('Home');
  await expect(agentTab).toBeFocused();
  await page.keyboard.press('End');
  await expect(strip.getByRole('tab', { name: 'Terminal' })).toBeFocused();
  await page.keyboard.press('Enter');
  await expect(page).toHaveURL(/show=term/);
  await expect(strip.getByRole('tab', { name: 'Terminal' })).toHaveAttribute('aria-selected', 'true');
  await page.keyboard.press('ArrowLeft');
  await page.keyboard.press('Enter');
  await expect(page).not.toHaveURL(/show=term/);
  await expect(log).toBeVisible();

  // Keys and quick replies live behind ⋯ in the composer.
  await page.getByRole('button', { name: 'Keys and quick replies' }).click();
  await expect(page.getByRole('region', { name: 'Keys and quick replies' })).toBeVisible();
  await page.getByRole('button', { name: 'Keys and quick replies' }).click();

  // + menu offers a new agent / terminal.
  await page.getByRole('button', { name: 'New tab' }).click();
  await expect(page.getByRole('menuitem', { name: 'New terminal' })).toBeVisible();
  await page.keyboard.press('Escape');

  // The Terminal tab mirrors the pane.
  await page.getByRole('tab', { name: 'Terminal' }).click();
  await expect(page).toHaveURL(/show=term/);
  await expect(page.locator('.term').first()).toBeVisible();
  await expect(log).toHaveCount(0);
  await page.getByRole('tab', { name: /claude|Claude/ }).first().click();
  await expect(log).toBeVisible();

  // Another agent waiting for an approval: the card sits at the end of its stream.
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'api-auth' }).click();
  const card = page.locator('[data-nav-item]').filter({ hasText: 'cargo test -p auth' }).first();
  await expect(card).toBeVisible({ timeout: 30_000 });
  await expect(card.locator('[data-act="allow"]')).toBeVisible();
  await expect(card.locator('[data-act="deny"]')).toBeVisible();
  await settled(page);
  await shoot(app, page, 'conversation-approval');

  // Phone size: the same conversation full width, title / repo · host header.
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'homepage' }).click();
  await expect(log.getByText('Ready for review.')).toBeVisible({ timeout: 20_000 });
  await resize(page, 390, 844);
  await expect(sidebar).toHaveCount(0);
  await expect(page.getByRole('button', { name: /Open sidebar/ }).first()).toBeVisible();
  await expect(log.getByText('Ready for review.')).toBeVisible();
  await page.mouse.move(2, 400); // no hover highlight on a row in the capture
  await settled(page);
  await shoot(app, page, 'conversation-mobile');
  await resize(page, 1440, 900);
});
