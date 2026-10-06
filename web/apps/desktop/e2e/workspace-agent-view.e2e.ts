// Agent view (conversation ↔ terminal) against a real server + gateway: the tab-strip toggle
// switches a workspace's agent to its terminal (mirror, key belt, composer typing into the pane
// with pane.send_text) and back; ⌘⇧T and the palette do the same; the Settings default applies to
// a workspace without an override, an override stays until "Use default". Light/dark captures at
// 1440×900 (terminal view, Settings → Appearance) and 390×844 (terminal view).

import { execFileSync } from 'node:child_process';
import { copyFileSync, existsSync, mkdirSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page } from '@playwright/test';
import { TestHost, appRoot, built, hasDisplay, hookEvent, launchApp, settled, shoot, vibekeBin, type LaunchedApp } from './helpers';

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

/** Wait until the pane's shell runs commands (it echoes a marker back). */
async function shellReady(pane: string): Promise<void> {
  const marker = `VK-READY-${Date.now()}`;
  host.cli(['pane', 'send-text', pane, `echo ${marker}`]);
  host.cli(['pane', 'send-keys', pane, 'enter']);
  await host.until(() => host.cli(['pane', 'read', pane]).split(marker).length > 2, 20_000, 'shell did not start');
}

/** A workspace whose shell has an agent run (SessionStart / prompt / Stop hooks), then a tidy screen. */
async function agentWorkspace(name: string, prompt: string): Promise<{ pane: string; cwd: string }> {
  const cwd = join(host.root, name);
  mkdirSync(cwd, { recursive: true });
  execFileSync('git', ['init', '-q', '-b', 'main'], { cwd, env: host.env, stdio: 'ignore' });
  writeFileSync(join(cwd, 'README.md'), `# ${name}\n`);
  const r = JSON.parse(host.cli(['workspace', 'create', '--cwd', cwd, '--name', name]));
  const w = { pane: r.root_pane.handle as string, cwd };
  // A short transcript (prompt, a tool call, the answer) so the conversation has something to show.
  const transcript = join(host.root, `${name}.jsonl`);
  const ts = (s: number) => new Date(Date.UTC(2026, 9, 6, 9, 0, s)).toISOString();
  writeFileSync(
    transcript,
    [
      { type: 'user', timestamp: ts(0), message: { role: 'user', content: prompt } },
      { type: 'assistant', timestamp: ts(4), message: { role: 'assistant', content: [{ type: 'tool_use', id: 'toolu_1', name: 'Read', input: { file_path: `${cwd}/README.md` } }] } },
      { type: 'user', timestamp: ts(5), message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: 'toolu_1', content: `# ${name}` }] } },
      { type: 'assistant', timestamp: ts(12), message: { role: 'assistant', content: [{ type: 'text', text: `Done: the ${name} README now has a getting-started section.` }] } },
    ]
      .map((l) => JSON.stringify(l))
      .join('\n') + '\n',
  );
  await shellReady(w.pane);
  // The run has picked up the transcript and finished its turn (keystrokes sent while the shell
  // starts can be lost, so the hooks are sent again until it has).
  const hasRun = () =>
    (JSON.parse(host.cli(['agent', 'list'])).runs as { transcript_path: string | null; turns_completed: number }[]).some((r) => r.transcript_path === transcript && r.turns_completed > 0);
  for (let i = 0; i < 3 && !hasRun(); i++) {
    hookEvent(host, w.pane, 'SessionStart', w.cwd, { source: 'startup', transcript_path: transcript });
    hookEvent(host, w.pane, 'UserPromptSubmit', w.cwd, { prompt });
    hookEvent(host, w.pane, 'Stop', w.cwd);
    await host.until(hasRun, 10_000, 'run pending').catch(() => {});
  }
  expect(hasRun()).toBe(true);
  // A screen that reads like an agent's own interface (scrolled clear: `clear` would end the run).
  const lines = [`✻ ${name} · session ready`, '', '  Type a message, or /help for commands.', '  Esc to interrupt · Shift+Tab to change mode', ''];
  host.cli(['pane', 'send-text', w.pane, `PS1='$ '; printf '${'\\n'.repeat(40)}'; printf '%s\\n' ${lines.map((l) => `'${l}'`).join(' ')}`]);
  host.cli(['pane', 'send-keys', w.pane, 'enter']);
  // Drawn once the command line has scrolled off the visible screen and the text is there.
  await host.until(() => {
    const screen = host.cli(['pane', 'read', w.pane]);
    return screen.includes('session ready') && !screen.includes('printf');
  }, 10_000, 'screen not drawn');
  return w;
}

async function resize(page: Page, width: number, height: number) {
  await a!.app.evaluate(({ BrowserWindow }, [w, h]) => {
    const win = BrowserWindow.getAllWindows().find((x) => x.webContents.getURL().includes('surface=full'));
    win?.setContentSize(w!, h!);
  }, [width, height]);
  await page.waitForFunction(([w]) => window.innerWidth === w, [width], { timeout: 5000 }).catch(() => page.setViewportSize({ width, height }));
}

/** Copy captures to VIBEKE_E2E_SHOTS_DIR when set (review folder). */
function exportShots(names: string[]): void {
  const dir = process.env.VIBEKE_E2E_SHOTS_DIR;
  if (!dir) return;
  mkdirSync(dir, { recursive: true });
  for (const n of names)
    for (const m of ['light', 'dark']) {
      const f = join(appRoot, 'test-results', `${n}-${m}.png`);
      if (existsSync(f)) copyFileSync(f, join(dir, `${n}-${m}.png`));
    }
}

test('agent view: toggle, typing into the terminal, shortcut, palette, settings default and override', async () => {
  test.setTimeout(240_000);
  const hero = await agentWorkspace('homepage', 'Rebuild the homepage hero');
  await agentWorkspace('docs-site', 'Draft the getting-started page');

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

  const strip = page.getByRole('tablist', { name: 'Tabs' });
  const log = page.getByRole('log', { name: 'Conversation' });
  const screen = page.locator('[data-terminal-screen]');
  const showTerminal = page.getByRole('button', { name: 'Show terminal' });
  const showConversation = page.getByRole('button', { name: 'Show conversation' });

  // Default: the conversation, a secondary Terminal tab, the toggle on Conversation.
  await expect(log).toBeVisible({ timeout: 30_000 });
  await expect(strip.getByRole('tab', { name: 'Terminal' })).toBeVisible();
  await expect(showConversation).toHaveAttribute('aria-pressed', 'true');
  await expect(showTerminal).toHaveAttribute('aria-pressed', 'false');

  // Toggle to the terminal: the mirror, the key belt, a composer that types into the pane.
  await showTerminal.click();
  await expect(screen).toBeVisible();
  await expect(log).toHaveCount(0);
  await expect(screen).toContainText('session ready', { timeout: 15_000 });
  await expect(showTerminal).toHaveAttribute('aria-pressed', 'true');
  await expect(strip.getByRole('tab', { name: 'Conversation' })).toBeVisible();
  await expect(strip.getByRole('tab', { name: 'Terminal' })).toHaveCount(0);
  await expect(strip.getByRole('tab', { selected: true })).not.toHaveText('Conversation');
  await expect(strip.getByRole('tab', { selected: true })).toHaveAttribute('data-tab', /^a:/);
  await expect(page).not.toHaveURL(/show=/);
  await expect(page.locator('[data-belt]').getByRole('button', { name: 'Keys' })).toBeVisible();
  const box = page.getByRole('textbox', { name: 'Type into the agent’s terminal' });
  await expect(box).toBeVisible();
  await expect(box).toBeFocused();
  const marker = `VK-TYPED-${Date.now()}`;
  await box.fill(`echo ${marker}-ok`);
  await page.getByRole('button', { name: 'Send', exact: true }).click();
  // The shell ran it (pane.send_text + Enter): the command and its output are on the screen.
  await host.until(() => host.cli(['pane', 'read', hero.pane]).split(`${marker}-ok`).length > 2, 15_000, 'typed text did not reach the pane');
  await expect(screen).toContainText(`${marker}-ok`, { timeout: 10_000 });
  // Clicking the screen hands typing focus back to the composer.
  await page.locator('[data-belt]').getByRole('button', { name: 'Keys' }).click();
  await expect(page.locator('[data-belt]').getByText('Chord')).toBeVisible();
  await page.locator('[data-belt]').getByRole('button', { name: 'Keys' }).click();
  await screen.click({ position: { x: 300, y: 300 } });
  await expect(box).toBeFocused();
  // The view is remembered per workspace: away and back, still the terminal.
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'docs-site' }).click();
  await expect(log).toBeVisible({ timeout: 30_000 });
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'homepage' }).click();
  await expect(screen).toBeVisible();
  await page.mouse.move(5, 450);
  await box.blur();
  await settled(page);
  await shoot(app, page, 'agent-view-terminal');

  // ⌘⇧T flips back and forth.
  await page.keyboard.press(`${mod}+Shift+T`);
  await expect(log).toBeVisible();
  await expect(showConversation).toHaveAttribute('aria-pressed', 'true');
  await page.keyboard.press(`${mod}+Shift+T`);
  await expect(screen).toBeVisible();

  // The secondary tab shows the other view and says so in the URL.
  await strip.getByRole('tab', { name: 'Conversation' }).click();
  await expect(page).toHaveURL(/show=conversation/);
  await expect(log).toBeVisible();
  await strip.getByRole('tab', { selected: false }).first().click();
  await expect(screen).toBeVisible();

  // Palette: both commands; run "Show agent as conversation".
  await page.keyboard.press(`${mod}+K`);
  const palette = page.getByRole('combobox', { name: /Jump to a pane/ });
  await palette.fill('show agent as');
  await expect(page.getByRole('option', { name: /Show agent as terminal/ })).toBeVisible();
  await page.getByRole('option', { name: /Show agent as conversation/ }).click();
  await expect(log).toBeVisible();
  await expect(showConversation).toHaveAttribute('aria-pressed', 'true');

  // Settings default → Terminal: docs-site (no override) follows, homepage keeps its override.
  await page.keyboard.press(`${mod}+4`);
  await expect(page).toHaveURL(/#\/settings/);
  const agentView = page.getByRole('radiogroup', { name: 'Agent view' });
  await expect(agentView.getByRole('radio', { name: 'Conversation' })).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByText('Conversation shows a readable summary')).toBeVisible();
  await agentView.getByRole('radio', { name: 'Terminal' }).click();
  await expect(agentView.getByRole('radio', { name: 'Terminal' })).toHaveAttribute('aria-checked', 'true');
  await expect(page.getByText('Set differently in 1 workspace')).toBeVisible();
  await page.mouse.move(5, 450);
  await settled(page);
  await shoot(app, page, 'agent-view-settings');

  await sidebar.locator('[data-nav-item]').filter({ hasText: 'docs-site' }).click();
  await expect(screen).toBeVisible({ timeout: 30_000 });
  await expect(screen).toContainText('session ready', { timeout: 15_000 });
  await expect(strip.getByRole('tab', { name: 'Conversation' })).toBeVisible();
  await sidebar.locator('[data-nav-item]').filter({ hasText: 'homepage' }).click();
  await expect(log).toBeVisible({ timeout: 30_000 });
  // "Use default view (Terminal)" clears the override.
  await page.getByRole('button', { name: 'Workspace options' }).click();
  await page.getByRole('menuitem', { name: 'Use default view (Terminal)' }).click();
  await expect(screen).toBeVisible();
  await page.getByRole('button', { name: 'Workspace options' }).click();
  await expect(page.getByRole('menuitem', { name: /Use default view/ })).toHaveCount(0);
  await page.keyboard.press('Escape');

  // Phone size: the terminal view full width with the belt and composer.
  await resize(page, 390, 844);
  await expect(sidebar).toHaveCount(0);
  await expect(screen).toBeVisible();
  await expect(page.getByRole('button', { name: 'Show conversation' })).toBeVisible();
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.mouse.move(2, 400);
  await settled(page);
  await shoot(app, page, 'agent-view-terminal-mobile');
  await resize(page, 1440, 900);

  // Back to the default for later runs on this profile (none persist, but keep it tidy).
  await page.keyboard.press(`${mod}+4`);
  await page.getByRole('radiogroup', { name: 'Agent view' }).getByRole('radio', { name: 'Conversation' }).click();
  exportShots(['agent-view-terminal', 'agent-view-settings', 'agent-view-terminal-mobile']);
});
