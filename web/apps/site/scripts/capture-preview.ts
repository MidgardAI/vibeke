// Record the real TUI and the paired phone app for the narrated home page preview.
// One isolated sample session: an agent pane asks for permission, the paired PWA allows it, and
// the pane continues. Agent output in the panes is scripted sample text; the approval is real.
// Writes intermediate captures (not published) for scripts/render-preview.ts.
// Requires target/debug/vibeke (or VIBEKE_BIN), a built PWA (or VIBEKE_PWA_DIST), tmux, and Chrome.
// Run from this directory: bun scripts/capture-preview.ts [out-dir]
import { execFileSync, spawn, type ChildProcess } from 'node:child_process'
import { existsSync, mkdirSync, writeFileSync } from 'node:fs'
import { hostname, userInfo } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { createServer } from 'node:net'
import { chromium, expect, type Browser } from '@playwright/test'
import { TestHost, hookEvent, settled } from '../../desktop/e2e/helpers'
import { parseAnsi } from '../../../packages/ui/src/lib/ansi'

const scriptDir = dirname(fileURLToPath(import.meta.url))
const repo = resolve(scriptDir, '../../../..')
const out = resolve(process.argv[2] ?? '/tmp/vibeke-preview-capture')
const bin = process.env.VIBEKE_BIN ?? join(repo, 'target/debug/vibeke')
const pwaDist = process.env.VIBEKE_PWA_DIST ?? join(repo, 'web/apps/pwa/dist')
const tmuxBin = existsSync('/opt/homebrew/bin/tmux') ? '/opt/homebrew/bin/tmux' : 'tmux'
const COLS = 112, ROWS = 32
const host = new TestHost(bin)
const children: ChildProcess[] = []
let browser: Browser | undefined
let recording = false, shooting = false
let loop: Promise<void> | undefined, shots: Promise<void> | undefined
const tmuxName = `vibeke-preview-${process.pid}`
const delay = (ms: number) => new Promise(r => setTimeout(r, ms))
const run = (args: string[]) => host.cli(args)
const api = (method: string, params: unknown = {}) => JSON.parse(run(['api', 'call', method, JSON.stringify(params)]))
const shq = (s: string) => `'${s.replaceAll("'", `'\\''`)}'`
const user = userInfo().username
// Every name this machine may show, longest first; the TUI may also truncate them.
const machineNames = [...new Set([hostname(), hostname().split('.')[0]!, ...['ComputerName', 'LocalHostName'].map(k => {
  try { return execFileSync('scutil', ['--get', k], { encoding: 'utf8' }).trim() } catch { return '' }
})])].filter(n => n.length >= 4).sort((a, b) => b.length - a.length)
const scrub = (s: string) => {
  for (const name of machineNames) for (let n = name.length; n >= 8; n--) s = s.replaceAll(name.slice(0, n), 'demo-laptop'.slice(0, n).padEnd(n, ' '))
  return s
}

/** Styled cells of one TUI screen, with the capture machine's host and user names replaced. */
function screen(ansi: string) {
  for (const name of machineNames) ansi = ansi.replace(new RegExp(`${name.split('.')[0]}:(\\S+) ${user}\\$`, 'g'), (m, dir) => `demo:${dir} $`.padEnd(m.length))
  ansi = scrub(ansi).replaceAll(user, 'demo'.padEnd(user.length, ' '))
  const plain = ansi.toLowerCase()
  if ([user, ...machineNames].some(n => plain.includes(n.toLowerCase().slice(0, 8)))) throw new Error('TUI capture still shows a local name')
  return parseAnsi(ansi).slice(0, ROWS).map(line => line.map(({ text, style }) => ({ text, fg: style.inverse ? style.bg ?? 'var(--term-bg)' : style.fg, bg: style.inverse ? style.fg ?? 'var(--term-fg)' : style.bg, bold: style.bold || undefined, dim: style.dim || undefined, italic: style.italic || undefined, underline: style.underline || undefined })))
}

function sampleWorkspace(name: string, branch: string) {
  const cwd = join(host.root, name)
  mkdirSync(join(cwd, 'src'), { recursive: true })
  const git = (...args: string[]) => execFileSync('git', ['-c', 'user.email=demo@example.com', '-c', 'user.name=Demo', ...args], { cwd, env: host.env, stdio: 'ignore' })
  git('init', '-q', '-b', branch)
  writeFileSync(join(cwd, 'README.md'), '# Sample workspace\n')
  git('add', '.'); git('commit', '-qm', 'Create sample workspace')
  const workspace = api('workspace.create', { cwd, name })
  return { name, cwd, pane: workspace.root_pane.handle as string, workspace: workspace.workspace.id as string }
}
type Ws = ReturnType<typeof sampleWorkspace>
async function readyShell(pane: string) {
  run(['pane', 'send-text', pane, 'echo CAPTURE_READY']); run(['pane', 'send-keys', pane, 'enter'])
  await host.until(() => run(['pane', 'read', pane]).split('CAPTURE_READY').length > 2, 15000, 'sample shell did not start')
}
function paneScript(ws: Ws, body: string) {
  const file = join(host.root, `pane-${ws.name}.sh`)
  writeFileSync(file, `clear\n${body}\nsleep 900\n`)
  run(['pane', 'send-text', ws.pane, `sh ${shq(file)}`]); run(['pane', 'send-keys', ws.pane, 'enter'])
}

const C = { mauve: '\\033[38;2;203;166;247m', green: '\\033[38;2;166;227;161m', yellow: '\\033[38;2;249;226;175m', blue: '\\033[38;2;137;180;250m', dim: '\\033[2m', bold: '\\033[1m', off: '\\033[0m' }
const COMMAND = 'bun test src/auth.test.ts'

/** Scripted agent output that blocks on a real Vibeke permission request (Claude hook). */
function agentPane(ws: Ws) {
  const decisionFile = join(host.root, 'permission-decision.json')
  const checkDecision = 'const response = await Bun.stdin.json(); process.exit(response.hookSpecificOutput?.decision?.behavior === "allow" ? 0 : 1)'
  const payload = JSON.stringify({ session_id: 'preview-dashboard', hook_event_name: 'PermissionRequest', tool_name: 'Bash', tool_input: { command: COMMAND, description: 'Run the auth tests' }, cwd: ws.cwd })
  const head = `${C.mauve}Claude Code${C.off}  ·  dashboard\\n\\n❯ Fix the token refresh race.\\n\\n`
  const steps = ['Read src/auth.ts', 'Edit src/auth.ts', 'Add src/auth.test.ts']
  const box = `${C.yellow}╭─ Bash command ──────────────────────╮${C.off}\\n${C.yellow}│${C.off} ${COMMAND.padEnd(36)}${C.yellow}│${C.off}\\n${C.yellow}╰─────────────────────────────────────╯${C.off}\\n${C.bold}Do you want to proceed?${C.off}\\n${C.blue}❯ 1. Yes${C.off}\\n  2. No\\n`
  paneScript(ws, [
    `printf '${head}'`,
    ...steps.flatMap(s => ['sleep 0.9', `printf '${C.green}✓${C.off} ${s}\\n'`]),
    `while [ ! -e ${shq(join(host.root, 'go'))} ]; do sleep 0.1; done`,
    `printf '\\n${box}'`,
    `echo ${shq(payload)} | ${shq(bin)} hook claude PermissionRequest >${shq(decisionFile)} 2>/dev/null || exit 1`,
    `${shq(process.execPath)} -e ${shq(checkDecision)} <${shq(decisionFile)} || exit 1`,
    `printf '\\033[2J\\033[H${head}' && printf '${C.green}✓${C.off} %s\\n' ${steps.map(shq).join(' ')}`,
    `printf '\\n  $ ${COMMAND}\\n'`,
    'sleep 1.2',
    `printf '\\n${C.green}  12 pass   0 fail${C.off}\\n'`,
    'sleep 0.7',
    `printf '\\nThe token refresh race is fixed.\\nReady for review.\\n'`,
  ].join('\n'))
}

try {
  mkdirSync(join(out, 'phone'), { recursive: true })
  mkdirSync(join(host.env.XDG_CONFIG_HOME!, 'vibeke'), { recursive: true })
  writeFileSync(join(host.env.XDG_CONFIG_HOME!, 'vibeke/config.toml'), 'onboarding = false\n')
  const probe = createServer()
  await new Promise<void>(r => probe.listen(0, '127.0.0.1', r))
  const port = (probe.address() as { port: number }).port
  await new Promise<void>(r => probe.close(() => r()))
  const origin = `http://127.0.0.1:${port}`
  writeFileSync(join(host.gatewayDir, 'gateway.toml'), `relay = "${origin}"\napp_url = "${origin}"\nhost_name = "demo-laptop"\nsession = "t"\n`)
  children.push(spawn(bin, ['relay', '--listen', `127.0.0.1:${port}`, '--public-url', origin, '--app-dir', pwaDist], { env: host.env, stdio: 'ignore' }))
  await host.start()
  console.log('Isolated server, gateway, and relay ready')

  const ws: Record<string, Ws> = {}
  for (const [name, branch] of [['dashboard', 'fix/token-refresh'], ['runtime', 'feat/session-recovery'], ['documentation', 'docs/quickstart']] as const) ws[name] = sampleWorkspace(name, branch)
  for (const w of Object.values(ws)) await readyShell(w.pane)
  hookEvent(host, ws.documentation!.pane, 'SessionStart', ws.documentation!.cwd, { source: 'startup' })
  await delay(500)
  hookEvent(host, ws.documentation!.pane, 'UserPromptSubmit', ws.documentation!.cwd, { prompt: 'Update the quickstart commands' })
  hookEvent(host, ws.runtime!.pane, 'SessionStart', ws.runtime!.cwd, { source: 'startup' })
  await delay(1500)
  paneScript(ws.runtime!, `printf '${C.mauve}Claude Code${C.off}  ·  runtime\\n\\n❯ Keep the agent session running after a disconnect.\\n\\n${C.green}✓${C.off} Update holder reconnect path\\n${C.green}✓${C.off} Add src/session.test.ts\\n\\n${C.green}  4 pass   0 fail${C.off}\\n\\nReady for review.\\n'`)
  paneScript(ws.documentation!, `printf '${C.mauve}Claude Code${C.off}  ·  documentation\\n\\n❯ Update the quickstart commands.\\n\\n${C.green}✓${C.off} Read docs/quickstart.md\\n${C.dim}✻ Writing…${C.off}\\n'`)

  // Pair the phone first so it is ready on the inbox when the request arrives.
  browser = await chromium.launch(process.platform === 'darwin' ? { channel: 'chrome' } : {})
  // A phone user agent, so the host names the paired device as a phone.
  const userAgent = 'Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/130.0.0.0 Mobile Safari/537.36'
  const context = await browser.newContext({ userAgent, viewport: { width: 400, height: 820 }, deviceScaleFactor: 3, colorScheme: 'dark', isMobile: true, hasTouch: true })
  const phone = await context.newPage()
  let pairOutput = ''
  const pairing = spawn(bin, ['gateway', 'pair', '--no-qr', '--no-confirm'], { env: host.env, stdio: ['ignore', 'pipe', 'ignore'] })
  children.push(pairing)
  pairing.stdout?.on('data', chunk => { pairOutput += String(chunk) })
  await host.until(() => pairOutput.includes('/#/pair?d='), 15000, 'pair link not ready')
  await phone.goto(pairOutput.match(/http:\/\/[^\s]+\/#\/pair\?d=[^\s]+/)![0])
  await phone.getByRole('button', { name: 'Pair', exact: true }).click()
  await expect(phone.getByRole('button', { name: 'Open Vibeke' })).toBeVisible({ timeout: 30000 })
  await phone.getByRole('button', { name: 'Open Vibeke' }).click()
  await phone.getByRole('button', { name: 'Skip', exact: true }).click()
  await phone.goto(`${origin}/#/inbox`)
  await settled(phone)
  console.log('Phone paired')

  // The TUI, attached through tmux and sampled whenever the screen changes.
  api('workspace.focus', { workspace: ws.dashboard!.workspace })
  const tmux = (...args: string[]) => execFileSync(tmuxBin, ['-L', tmuxName, ...args], { env: host.env, cwd: ws.dashboard!.cwd, encoding: 'utf8' })
  tmux('-f', '/dev/null', 'new-session', '-d', '-s', 'capture', '-x', String(COLS), '-y', String(ROWS), bin, '--session', 't')
  tmux('set-option', '-g', 'status', 'off')
  await delay(2500)
  const start = Date.now()
  const now = () => Date.now() - start
  const tui: { t: number; cells: ReturnType<typeof screen> }[] = []
  const marks: Record<string, number> = {}
  let last = ''
  recording = true
  loop = (async () => {
    while (recording) {
      const ansi = tmux('capture-pane', '-p', '-e', '-t', 'capture')
      if (ansi !== last) { last = ansi; tui.push({ t: now(), cells: screen(ansi) }) }
      await delay(30)
    }
  })()
  await delay(1200)
  marks.agent = now()
  agentPane(ws.dashboard!)
  await host.until(() => run(['pane', 'read', ws.dashboard!.pane]).includes('Add src/auth.test.ts'), 20000, 'agent did not start')
  await delay(1600)
  // The user moves on to another workspace. A pane that no terminal client is focused on keeps
  // its permission request open for an answer from elsewhere (04 §7.2).
  marks.away = now()
  const keys = async (...k: string[]) => { tmux('send-keys', '-t', 'capture', ...k); await delay(150) }
  await keys('C-b'); await keys('w'); await delay(500); await keys('/')
  for (const ch of 'runtime') await keys('-l', ch)
  await delay(300); await keys('Enter'); await keys('Enter')
  await delay(1400)
  if (!last.includes('4 pass')) { console.error(scrub(last)); throw new Error('TUI did not switch to the runtime workspace') }
  writeFileSync(join(host.root, 'go'), '')
  await host.until(() => host.interactions('open').length >= 1, 20000, 'approval did not open')
  marks.request = now()
  const request = host.interactions('open')[0]!
  if (!(request as { gate?: boolean }).gate) throw new Error('The request was not held for an answer from the phone')
  await expect(phone.getByText(COMMAND).first()).toBeVisible({ timeout: 20000 })
  await settled(phone)
  await delay(2500)
  await phone.screenshot({ path: join(out, 'phone/before.png') })
  marks.phoneBefore = now()

  // The tap and its transition: full-resolution screenshots of the top of the screen (where the
  // request card is), kept whenever they change, on the same clock as the TUI.
  const phoneFrames: { t: number; file: string }[] = []
  shooting = true
  let previous: Buffer | undefined
  shots = (async () => {
    while (shooting) {
      const t = now()
      const png = await phone.screenshot({ clip: { x: 0, y: 0, width: 400, height: 330 } })
      if (!previous?.equals(png)) {
        previous = png
        const file = `phone/${String(phoneFrames.length + 1).padStart(4, '0')}.png`
        writeFileSync(join(out, file), png)
        phoneFrames.push({ t, file })
      }
    }
  })()
  await delay(600)
  const allow = phone.getByRole('button', { name: /^Allow$/ }).first()
  const box = (await allow.boundingBox())!
  marks.tap = now()
  await allow.tap()
  await host.until(() => run(['pane', 'read', ws.dashboard!.pane]).includes('Ready for review.'), 20000, 'agent did not continue')
  const answered = host.interactions().find(i => i.id === request.id && i.answer?.decision?.toLowerCase() === 'allow')
  if (!answered) throw new Error('The phone approval was not recorded as allow')
  marks.continued = now()
  await delay(1500)
  shooting = false
  await shots
  await settled(phone)
  await phone.screenshot({ path: join(out, 'phone/after.png') })
  marks.back = now()
  await keys('C-b'); await keys('L')
  await delay(3000)
  marks.end = now()
  recording = false
  await loop

  writeFileSync(join(out, 'capture.json'), JSON.stringify({
    capturedAt: new Date().toISOString(), sampleData: true,
    note: 'Live TUI in a tmux PTY and the actual PWA paired through a local relay. Agent output is scripted sample text; the permission request and approval are real.',
    tuiSize: [COLS, ROWS], phoneViewport: [400, 820], phoneScale: 3, tap: { x: box.x + box.width / 2, y: box.y + box.height / 2 },
    marks, tui, phone: phoneFrames,
  }))
  console.log(`Wrote ${tui.length} TUI screens and ${phoneFrames.length} phone frames to ${out}`, marks)
} finally {
  recording = false
  shooting = false
  await Promise.allSettled([loop, shots])
  try { execFileSync(tmuxBin, ['-L', tmuxName, 'kill-server'], { stdio: 'ignore' }) } catch {}
  await browser?.close()
  for (const child of children) child.kill('SIGTERM')
  await host.stop()
  for (const child of children) if (child.exitCode === null) child.kill('SIGKILL')
}
