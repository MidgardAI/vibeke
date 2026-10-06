// Capture the real TUI, Electron renderer, and PWA against one isolated sample session.
// Requires built desktop/PWA apps, target/debug/vibeke, tmux, and installed Chrome.
// Run from this directory: bunx tsx scripts/capture-clients.ts
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { mkdirSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createServer } from 'node:net'
import { chromium, expect, type Browser } from '@playwright/test'
import { TestHost, launchApp, hookEvent, settled, appearance, type LaunchedApp } from '../../desktop/e2e/helpers'
import { parseAnsi } from '../../../packages/ui/src/lib/ansi'

const scriptDir = dirname(fileURLToPath(import.meta.url))
const repo = resolve(scriptDir, '../../../..')
const output = resolve(scriptDir, '../public/screenshots')
const bin = join(repo, 'target/debug/vibeke')
const host = new TestHost(bin)
const children: ChildProcess[] = []
let desktop: LaunchedApp | undefined
let browser: Browser | undefined
const tmuxName = `vibeke-shots-${process.pid}`
const delay = (ms: number) => new Promise(resolve => setTimeout(resolve, ms))
const run = (args: string[], input?: string) => host.cli(args, input)
const api = (method: string, params: unknown = {}) => JSON.parse(run(['api', 'call', method, JSON.stringify(params)]))
const escape = (s: string) => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;').replaceAll('"', '&quot;')

function sampleWorkspace(name: string, branch: string) {
  const cwd = join(host.root, name)
  mkdirSync(join(cwd, 'src'), { recursive: true })
  const git = (...args: string[]) => execFileSync('git', ['-c', 'user.email=demo@example.com', '-c', 'user.name=Demo', ...args], { cwd, env: host.env, stdio: 'ignore' })
  git('init', '-q', '-b', branch)
  writeFileSync(join(cwd, 'src/session.ts'), 'export function reconnect(id: string) {\n  return sessions.get(id);\n}\n')
  writeFileSync(join(cwd, 'README.md'), '# Sample workspace\n')
  git('add', '.'); git('commit', '-qm', 'Create sample workspace')
  writeFileSync(join(cwd, 'src/session.ts'), 'export async function reconnect(id: string) {\n  const holder = await holders.find(id);\n  const checkpoint = await holder.checkpoint();\n  return holder.attach({ after: checkpoint.sequence });\n}\n')
  writeFileSync(join(cwd, 'src/session.test.ts'), 'test("reconnect preserves the session", async () => {\n  const session = await reconnect("builder");\n  expect(session.attached).toBe(true);\n});\n')
  const workspace = api('workspace.create', { cwd, name })
  return { cwd, pane: workspace.root_pane.handle, workspace: workspace.workspace.id }
}
async function readyShell(pane: string) {
  run(['pane', 'send-text', pane, 'echo CAPTURE_READY']); run(['pane', 'send-keys', pane, 'enter'])
  await host.until(() => run(['pane', 'read', pane]).split('CAPTURE_READY').length > 2, 15000, 'sample shell did not start')
}
function transcript(path: string, cwd: string) {
  const lines: unknown[] = []
  const at = (n: number) => new Date(Date.now() - 120000 + n * 1000).toISOString()
  lines.push({ type: 'user', timestamp: at(0), message: { role: 'user', content: 'Keep the agent session running when the terminal disconnects. Add a test for reconnecting.' } })
  lines.push({ type: 'assistant', timestamp: at(4), message: { role: 'assistant', content: [{ type: 'text', text: 'I will check the session and holder code, then add the recovery test.' }] } })
  let n = 1
  for (const [name, input, result] of [
    ['Read', { file_path: `${cwd}/src/session.ts` }, 'Read session.ts'],
    ['Edit', { file_path: `${cwd}/src/session.ts`, old_string: 'return sessions.get(id);', new_string: 'return holder.attach({ after: checkpoint.sequence });' }, 'Updated session.ts'],
    ['Write', { file_path: `${cwd}/src/session.test.ts`, content: 'test("reconnect preserves the session", async () => { /* … */ });' }, 'Created session.test.ts'],
    ['Bash', { command: 'bun test src/session.test.ts', description: 'Run session tests' }, '4 pass\n0 fail'],
  ] as const) {
    const id = `sample-tool-${n}`
    lines.push({ type: 'assistant', timestamp: at(8 + n * 10), message: { role: 'assistant', content: [{ type: 'tool_use', id, name, input }] } })
    lines.push({ type: 'user', timestamp: at(9 + n++ * 10), message: { role: 'user', content: [{ type: 'tool_result', tool_use_id: id, content: result, is_error: false }] } })
  }
  lines.push({ type: 'assistant', timestamp: at(65), message: { role: 'assistant', content: [{ type: 'text', text: 'The session now reconnects to its holder.\n\n- The agent keeps running after a disconnect.\n- The terminal restores output from the last checkpoint.\n- All four recovery tests pass.\n\nReady for review.' }] } })
  writeFileSync(path, lines.map(line => JSON.stringify(line)).join('\n') + '\n')
}

try {
  mkdirSync(output, { recursive: true })
  const probe = createServer()
  await new Promise<void>(resolve => probe.listen(0, '127.0.0.1', resolve))
  const port = (probe.address() as { port: number }).port
  await new Promise<void>(resolve => probe.close(() => resolve()))
  const origin = `http://127.0.0.1:${port}`
  writeFileSync(join(host.gatewayDir, 'gateway.toml'), `relay = "${origin}"\napp_url = "${origin}"\nhost_name = "devbox"\nsession = "t"\n`)
  const relay = spawn(bin, ['relay', '--listen', `127.0.0.1:${port}`, '--public-url', origin, '--app-dir', join(repo, 'web/apps/pwa/dist')], { env: host.env, stdio: 'ignore' })
  children.push(relay)
  await host.start()
  console.log('Isolated server, gateway, and relay ready')
  const core = sampleWorkspace('runtime', 'feat/session-recovery')
  const auth = sampleWorkspace('dashboard', 'fix/token-refresh')
  const docs = sampleWorkspace('documentation', 'docs/quickstart')
  const apiWork = sampleWorkspace('api', 'fix/request-timeout')
  const website = sampleWorkspace('website', 'feat/workspace-list')
  for (const workspace of [core, auth, docs, apiWork, website]) await readyShell(workspace.pane)
  const source = join(host.root, 'sample-transcript.jsonl')
  transcript(source, core.cwd)
  const picked = () => api('agent.list').runs.some((run: any) => run.transcript_path === source && run.turns_completed > 0)
  for (let attempt = 0; attempt < 3 && !picked(); attempt++) {
    hookEvent(host, core.pane, 'SessionStart', core.cwd, { source: 'startup', transcript_path: source, model: 'opus', permission_mode: 'default' })
    await delay(500)
    hookEvent(host, core.pane, 'UserPromptSubmit', core.cwd, { prompt: 'Keep the agent session running after a disconnect' })
    await delay(500)
    hookEvent(host, core.pane, 'Stop', core.cwd)
    await host.until(picked, 6000, 'hooks pending').catch(() => {})
  }
  if (!picked()) {
    console.log(JSON.stringify(api('agent.list')))
    console.log(run(['pane', 'read', core.pane]))
    throw new Error('Sample transcript not loaded')
  }
  hookEvent(host, docs.pane, 'SessionStart', docs.cwd, { source: 'startup' })
  await delay(400)
  hookEvent(host, docs.pane, 'UserPromptSubmit', docs.cwd, { prompt: 'Update the quickstart commands' })
  host.requestApproval(auth.pane, 'bun test src/auth.test.ts', auth.cwd)
  host.requestApproval(apiWork.pane, 'cargo test -p api', apiWork.cwd)
  host.requestApproval(website.pane, 'bun run build', website.cwd)

  desktop = await launchApp({ ...host.env, VIBEKE_BIN: bin, HOME: process.env.HOME ?? host.env.HOME })
  const page = desktop.page
  await desktop.app.evaluate(({ BrowserWindow }) => BrowserWindow.getAllWindows().find(w => w.webContents.getURL().includes('surface=full'))?.setContentSize(1440, 900))
  await appearance(desktop.app, 'dark')
  await page.emulateMedia({ colorScheme: 'dark' })
  await page.getByRole('button', { name: /Connect to this (Mac|computer)/ }).click()
  await expect(page.getByText(/^Connected to /)).toBeVisible({ timeout: 30000 })
  await page.getByRole('button', { name: 'Open Vibeke' }).click()
  await page.getByRole('button', { name: 'Skip', exact: true }).click()
  await page.getByRole('navigation', { name: 'Workspaces' }).locator('[data-nav-item]').filter({ hasText: 'runtime' }).click()
  await expect(page.getByRole('log', { name: 'Conversation' })).toContainText('Ready for review.', { timeout: 20000 })
  await expect(page.getByRole('complementary', { name: 'Workspace panel' })).toContainText('session.ts')
  await settled(page)
  await page.mouse.move(1400, 880)
  await page.screenshot({ path: join(output, 'electron.png') })
  console.log('Captured Electron workspace')

  browser = await chromium.launch({ channel: 'chrome' })
  const context = await browser.newContext({ viewport: { width: 430, height: 860 }, deviceScaleFactor: 2, colorScheme: 'dark', isMobile: true, hasTouch: true })
  const web = await context.newPage()
  let pairOutput = ''
  const pairing = spawn(bin, ['gateway', 'pair', '--no-qr', '--no-confirm'], { env: host.env, stdio: ['ignore', 'pipe', 'ignore'] })
  children.push(pairing)
  pairing.stdout?.on('data', chunk => { pairOutput += String(chunk) })
  await host.until(() => pairOutput.includes('/#/pair?d='), 15000, 'pair link not ready')
  const pairUrl = pairOutput.match(/http:\/\/[^\s]+\/#\/pair\?d=[^\s]+/)![0]
  await web.goto(pairUrl)
  await web.getByRole('button', { name: 'Pair', exact: true }).click()
  await expect(web.getByRole('button', { name: 'Open Vibeke' })).toBeVisible({ timeout: 30000 })
  await web.getByRole('button', { name: 'Open Vibeke' }).click()
  await web.getByRole('button', { name: 'Skip', exact: true }).click()
  await web.goto(`${origin}/#/inbox`)
  await expect(web.getByText('bun test src/auth.test.ts', { exact: false }).first()).toBeVisible({ timeout: 20000 })
  await settled(web)
  await web.screenshot({ path: join(output, 'web.png') })
  console.log('Captured the paired web app')

  const terminalText = '\\033[2J\\033[H\\033[38;2;203;166;247mClaude Code\\033[0m  ·  runtime\\n\\n❯ Keep the agent session running after a disconnect.\\n  Add a test for reconnecting.\\n\\n\\033[38;2;166;227;161m✓\\033[0m Read src/session.ts\\n\\033[38;2;166;227;161m✓\\033[0m Update holder reconnect path\\n\\033[38;2;166;227;161m✓\\033[0m Add src/session.test.ts\\n\\n  $ bun test src/session.test.ts\\n\\n\\033[38;2;166;227;161m  4 pass   0 fail\\033[0m\\n\\nThe session now reconnects to its holder.\\nThe agent keeps running after a disconnect.\\n\\nReady for review.\\n'
  const terminalScript = join(host.root, 'terminal-sample.sh')
  writeFileSync(terminalScript, `printf '${terminalText}'\nsleep 180\n`)
  run(['pane', 'send-text', core.pane, `/bin/sh '${terminalScript}'`]); run(['pane', 'send-keys', core.pane, 'enter'])
  api('workspace.focus', { workspace: core.workspace })
  const tmux = (...args: string[]) => execFileSync('/opt/homebrew/bin/tmux', ['-L', tmuxName, ...args], { env: host.env, cwd: core.cwd, encoding: 'utf8' })
  tmux('-f', '/dev/null', 'new-session', '-d', '-s', 'capture', '-x', '140', '-y', '42', bin, '--session', 't')
  tmux('set-option', '-g', 'status', 'off')
  await delay(2500)
  const ansi = tmux('capture-pane', '-p', '-e', '-t', 'capture')
  // Keep the terminal capture in memory; only the screenshot is published.
  const lines = parseAnsi(ansi).slice(0, 42)
  const html = lines.map(line => '<div>' + (line.map(({ text, style }) => {
    const css = [style.fg && `color:${style.fg}`, style.bg && `background:${style.bg}`, style.bold && 'font-weight:700', style.italic && 'font-style:italic', style.dim && 'opacity:.7'].filter(Boolean).join(';')
    return `<span style="${css}">${escape(text)}</span>`
  }).join('') || ' ') + '</div>').join('')
  const term = await browser.newPage({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1 })
  await term.setContent(`<html><head><style>:root{--ansi-black:#45475a;--ansi-red:#f38ba8;--ansi-green:#a6e3a1;--ansi-yellow:#f9e2af;--ansi-blue:#89b4fa;--ansi-magenta:#cba6f7;--ansi-cyan:#94e2d5;--ansi-white:#cdd6f4}body{margin:0;padding:30px;background:#1e1e2e;color:#cdd6f4}pre{margin:0;font:16px/20px Menlo,monospace;white-space:pre}</style></head><body><pre>${html}</pre></body></html>`)
  await term.screenshot({ path: join(output, 'tui.png') })
  console.log('Captured the live TUI through tmux')
  writeFileSync(join(output, 'capture.json'), JSON.stringify({ capturedAt: new Date().toISOString(), sourceCommit: execFileSync('git', ['rev-parse', 'HEAD'], { cwd: repo, encoding: 'utf8' }).trim(), sampleData: true, electron: 'Actual Electron renderer, 1440x870 at 2x', web: 'Actual PWA paired through a local encrypted relay, 430x860 at 2x', tui: 'Live TUI in a 140x42 PTY; ANSI capture rendered with original colors' }, null, 2) + '\n')
} finally {
  try { execFileSync('/opt/homebrew/bin/tmux', ['-L', tmuxName, 'kill-server'], { stdio: 'ignore' }) } catch {}
  await browser?.close()
  await desktop?.close()
  for (const child of children) child.kill('SIGTERM')
  await host.stop()
  for (const child of children) if (child.exitCode === null) child.kill('SIGKILL')
}
