// Record terminal-interface clips for the documentation: animated WebP files and PNG stills in
// docs/site/src/images/. Each clip runs the CLI in VIBEKE_BIN (use a released build) in a fresh,
// isolated session inside tmux, drives it with keys, and renders the captured screens in Chromium.
// Requires tmux, img2webp (libwebp) and the Playwright Chromium.
// Run from this directory: VIBEKE_BIN=/path/to/vibeke bunx tsx scripts/capture-docs.ts [clip ...]
import { execFileSync } from 'node:child_process'
import { mkdirSync, mkdtempSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'
import { chromium, type Page } from '@playwright/test'
import { parseAnsi } from '../../../packages/ui/src/lib/ansi'

const scriptDir = dirname(fileURLToPath(import.meta.url))
const repo = resolve(scriptDir, '../../../..')
const out = join(repo, 'docs/site/src/images')
const bin = process.env.VIBEKE_BIN ?? 'vibeke'
const tmuxBin = process.env.TMUX_BIN ?? '/opt/homebrew/bin/tmux'
const cols = 112, rows = 32
const delay = (ms: number) => new Promise(r => setTimeout(r, ms))
const escape = (s: string) => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')

// --- isolated host --------------------------------------------------------------------------

class Host {
  root = mkdtempSync('/tmp/vkdoc-')
  env: Record<string, string>
  project = join(this.root, 'home/code/your-project')
  sock = `vkdoc-${process.pid}-${Math.random().toString(36).slice(2, 6)}`
  constructor(onboarding: boolean) {
    const d = (n: string) => { const p = join(this.root, n); mkdirSync(p, { recursive: true }); return p }
    const home = d('home')
    // Stand-ins so setup detects agent CLIs; the clips never run them.
    const agents = d('agents')
    for (const name of ['claude', 'codex']) writeFileSync(join(agents, name), '#!/bin/sh\necho 1.0.0\n', { mode: 0o755 })
    this.env = {
      PATH: `${dirname(bin)}:${agents}:/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin`,
      HOME: home, USER: 'demo', SHELL: '/bin/zsh', TERM: 'xterm-256color', COLORTERM: 'truecolor', LANG: 'en_US.UTF-8',
      VIBEKE_RUNTIME_DIR: d('rt'), VIBEKE_GATEWAY_DIR: d('gw'), XDG_STATE_HOME: d('state'), XDG_CONFIG_HOME: d('cfg'),
      XDG_DATA_HOME: d('data'), TMPDIR: d('tmp'),
    }
    writeFileSync(join(home, '.zshrc'), "PS1='%F{blue}%1~%f $ '\nunsetopt PROMPT_SP\n")
    if (!onboarding) { mkdirSync(join(this.env.XDG_CONFIG_HOME!, 'vibeke')); writeFileSync(join(this.env.XDG_CONFIG_HOME!, 'vibeke/config.toml'), 'onboarding = false\n') }
    mkdirSync(join(this.project, 'src'), { recursive: true })
    writeFileSync(join(this.project, 'README.md'), '# your-project\n')
    writeFileSync(join(this.project, 'src/greet.ts'), 'export function greet(name: string) {\n  return `Hello, ${name}`;\n}\n')
    const git = (...a: string[]) => execFileSync('git', ['-c', 'user.email=demo@example.com', '-c', 'user.name=Demo', ...a], { cwd: this.project, env: this.env, stdio: 'ignore' })
    git('init', '-q', '-b', 'main'); git('add', '.'); git('commit', '-qm', 'Initial commit')
  }
  cli(...args: string[]) { return execFileSync(bin, args, { env: this.env, cwd: this.project, encoding: 'utf8', timeout: 20_000 }) }
  tmux(...args: string[]) { return execFileSync(tmuxBin, ['-L', this.sock, ...args], { env: this.env, cwd: this.project, encoding: 'utf8' }) }
  start() {
    this.tmux('-f', '/dev/null', 'new-session', '-d', '-s', 'c', '-x', String(cols), '-y', String(rows), 'vibeke')
    this.tmux('set-option', '-g', 'status', 'off')
  }
  screen() { return this.tmux('capture-pane', '-p', '-e', '-t', 'c') }
  text() { return this.tmux('capture-pane', '-p', '-t', 'c') }
  stop() {
    try { this.tmux('kill-server') } catch {}
    try { this.cli('server', 'stop', '--kill-panes') } catch {}
  }
}

// --- recording ------------------------------------------------------------------------------

interface Frame { t: number; ansi: string; caption: string }
class Clip {
  frames: Frame[] = []
  caption = ''
  private recording = false
  private start = 0
  private loop?: Promise<void>
  constructor(readonly host: Host) {}
  async until(text: string | RegExp, timeout = 15_000) {
    const end = Date.now() + timeout
    while (Date.now() < end) {
      const s = this.host.text()
      if (typeof text === 'string' ? s.includes(text) : text.test(s)) return
      await delay(100)
    }
    throw new Error(`timed out waiting for ${text}\n${this.host.text()}`)
  }
  record() {
    this.recording = true
    this.start = Date.now()
    this.loop = (async () => {
      while (this.recording) {
        const ansi = this.host.screen()
        const last = this.frames.at(-1)
        if (!last || last.ansi !== ansi || last.caption !== this.caption) this.frames.push({ t: Date.now() - this.start, ansi, caption: this.caption })
        await delay(80)
      }
    })()
  }
  async stop(hold = 2500) {
    await delay(hold)
    this.recording = false
    await this.loop
    this.frames.push({ ...this.frames.at(-1)!, t: Date.now() - this.start })
  }
  /** Press keys (tmux key names), showing `label` in the caption bar while they take effect. */
  async keys(label: string, ...k: string[]) {
    this.caption = label
    await delay(700)
    for (const key of k) { this.host.tmux('send-keys', '-t', 'c', key); await delay(140) }
    await delay(900)
  }
  async type(text: string, cps = 18) {
    for (const ch of text) { this.host.tmux('send-keys', '-t', 'c', '-l', ch); await delay(1000 / cps) }
  }
  pause = (ms: number) => delay(ms)
}

function html(ansi: string, caption: string) {
  // The sidebar header names the machine; show a neutral name of the same width.
  ansi = ansi.replace(/\/tmp\/vkdoc-\w+\/agents\/(\w+)/g, (m, agent: string) => `/usr/local/bin/${agent}`.padEnd(m.length, ' '))
  ansi = ansi.replace(/vibeke · (\S+)/g, (m, name: string) => 'vibeke · ' + 'laptop'.padEnd(name.length, ' '))
  const lines = parseAnsi(ansi).slice(0, rows)
  const body = lines.map(line => '<div>' + (line.map(({ text, style }) => {
    let fg = style.fg, bg = style.bg
    if (style.inverse) [fg, bg] = [bg ?? 'var(--bg)', fg ?? 'var(--fg)']
    const css = [fg && `color:${fg}`, bg && `background:${bg}`, style.bold && 'font-weight:700', style.italic && 'font-style:italic', style.dim && 'opacity:.6', style.underline && 'text-decoration:underline'].filter(Boolean).join(';')
    return `<span style="${css}">${escape(text)}</span>`
  }).join('') || ' ') + '</div>').join('')
  // "Ctrl+B i" shows two keys pressed in turn; a part in parentheses is a note after them.
  const parts = caption.match(/\([^)]*\)|\S+/g) ?? []
  const keys = parts.filter(p => !p.startsWith('(')).map(k => `<kbd>${escape(k)}</kbd>`).join('<span>then</span>')
  const note = parts.filter(p => p.startsWith('(')).map(p => `<span>${escape(p.slice(1, -1))}</span>`).join('')
  const cap = `<div class="cap">${keys}${note}</div>`
  return `<div id="shot"><div class="term">${body}</div>${cap}</div>`
}

const page_css = `
:root { --bg:#11111b; --fg:#cdd6f4;
  --ansi-black:#45475a; --ansi-red:#f38ba8; --ansi-green:#a6e3a1; --ansi-yellow:#f9e2af; --ansi-blue:#89b4fa; --ansi-magenta:#f5c2e7; --ansi-cyan:#94e2d5; --ansi-white:#bac2de;
  --ansi-bright-black:#585b70; --ansi-bright-red:#f38ba8; --ansi-bright-green:#a6e3a1; --ansi-bright-yellow:#f9e2af; --ansi-bright-blue:#89b4fa; --ansi-bright-magenta:#f5c2e7; --ansi-bright-cyan:#94e2d5; --ansi-bright-white:#a6adc8; }
body { margin:0; background:#0a0a0c; }
#shot { display:inline-block; background:var(--bg); padding:14px 16px 10px; border-radius:10px; }
.term { font:13px/17px Menlo, monospace; color:var(--fg); white-space:pre; width:${cols}ch; overflow:hidden; }
.term div { height:17px; line-height:17px; }
.term span { display:inline-block; height:17px; vertical-align:top; }
.cap { height:30px; margin-top:8px; display:flex; align-items:center; gap:8px; font:12px system-ui, sans-serif; color:#7f849c; }
kbd { font:600 12px Menlo, monospace; color:#cdd6f4; background:#313244; border:1px solid #45475a; border-radius:5px; padding:3px 8px; }
`

async function render(page: Page, frames: Frame[], name: string) {
  const dir = mkdtempSync('/tmp/vkdoc-frames-')
  const files: { file: string; ms: number }[] = []
  for (let i = 0; i < frames.length - 1; i++) {
    const f = frames[i]!
    const ms = Math.max(frames[i + 1]!.t - f.t, 40)
    const prev = files.at(-1)
    if (prev && i > 0 && frames[i - 1]!.ansi === f.ansi && frames[i - 1]!.caption === f.caption) { prev.ms += ms; continue }
    await page.setContent(`<style>${page_css}</style>${html(f.ansi, f.caption)}`)
    const file = join(dir, `${String(i).padStart(4, '0')}.png`)
    await page.locator('#shot').screenshot({ path: file })
    files.push({ file, ms })
  }
  const args = ['-loop', '0', '-lossy', '-q', '75', '-m', '6']
  for (const f of files) args.push('-d', String(Math.round(f.ms)), f.file)
  execFileSync('img2webp', [...args, '-o', join(out, `${name}.webp`)], { stdio: ['ignore', 'ignore', 'inherit'] })
  console.log(`  ${name}.webp: ${files.length} frames`)
}

async function still(page: Page, ansi: string, name: string) {
  await page.setContent(`<style>${page_css}</style>${html(ansi, '')}`)
  await page.locator('.term').evaluate(el => { (el.parentElement!.querySelector('.cap') as HTMLElement).remove() })
  await page.locator('#shot').screenshot({ path: join(out, `${name}.png`) })
  console.log(`  ${name}.png`)
}

// --- clips ----------------------------------------------------------------------------------

/** A pane script that looks like an agent asking to run a command, blocked on a real approval. */
function approvalScript(h: Host) {
  const file = join(h.root, 'agent.sh')
  const payload = JSON.stringify({ session_id: 'docs-demo', hook_event_name: 'PermissionRequest', tool_name: 'Bash', tool_input: { command: 'bun test src/greet.test.ts' }, cwd: h.project })
  const y = '\\033[33m', g = '\\033[32m', b = '\\033[1m', m = '\\033[35m', o = '\\033[0m'
  writeFileSync(file, [
    'clear',
    `printf '${m}agent${o}  ·  your-project\\n\\n❯ Add a test for greet()\\n\\n${g}✓${o} Read src/greet.ts\\n${g}✓${o} Created src/greet.test.ts\\n\\n'`,
    `printf '${y}Run command:${o} bun test src/greet.test.ts\\n${b}Allow?${o}\\n'`,
    `echo '${payload}' | vibeke hook claude PermissionRequest >/dev/null 2>&1`,
    // Without the agent's own prompt on screen there is nothing to deliver keys to: wait for the answer.
    `while vibeke interaction list --status open | grep -q 'bun test'; do sleep 0.3; done`,
    `printf '\\n  $ bun test src/greet.test.ts\\n'`, 'sleep 1', `printf '${g}  1 pass${o}  0 fail\\n\\nDone.\\n'`, 'sleep 600',
  ].join('\n') + '\n')
  return file
}

const clips: Record<string, (c: Clip, page: Page) => Promise<void>> = {
  async setup(c, page) {
    await c.until('Welcome to Vibeke')
    c.host.tmux('send-keys', '-t', 'c', 'Enter')
    await c.until('setup 2/5')
    await c.pause(800)
    await still(page, c.host.screen(), 'setup')
  },
  async 'key-menu'(c) {
    await c.until(/\$\s*$/m)
    c.record()
    await c.pause(800)
    await c.keys('Ctrl+B', 'C-b')
    await c.until(/pane|workspace/i)
    await c.pause(2200)
    await c.keys('v', 'v')
    await c.pause(800)
    await c.keys('Ctrl+B :', 'C-b', ':')
    await c.type('new tab')
    await c.pause(900)
    await c.keys('Enter', 'Enter')
    await c.until('command?')
    await c.keys('Enter (shell)', 'Enter')
    c.caption = ''
    await c.until(/\$\s*$/m)
    await c.type('ls\n')
    await c.stop()
  },
  async inbox(c) {
    await c.until(/\$\s*$/m)
    const script = approvalScript(c.host)
    c.host.tmux('send-keys', '-t', 'c', '-l', `sh ${script}`); c.host.tmux('send-keys', '-t', 'c', 'Enter')
    await c.until('Allow?')
    await c.pause(1500)
    c.record()
    await c.pause(1200)
    await c.keys('Ctrl+B i', 'C-b', 'i')
    await c.pause(1800)
    await c.keys('y (allow once)', 'y')
    await c.pause(1200)
    await c.keys('Esc', 'Escape')
    await c.until('Done.', 8000)
    c.caption = ''
    await c.stop(3000)
  },
  async 'new-task'(c) {
    await c.until(/\$\s*$/m)
    c.record()
    await c.pause(800)
    await c.keys('Ctrl+B Shift+K', 'C-b', 'K')
    await c.pause(500)
    await c.type('Add a test for greet')
    await c.pause(700)
    await c.keys('Enter', 'Enter')
    await c.pause(2500)
    c.caption = ''
    await c.type('git status\n')
    await c.stop(3000)
  },
}

const wanted = process.argv.slice(2).filter(a => !a.startsWith('-'))
const browser = await chromium.launch()
const page = await browser.newPage({ deviceScaleFactor: 2 })
mkdirSync(out, { recursive: true })
try {
  for (const [name, run] of Object.entries(clips)) {
    if (wanted.length && !wanted.includes(name)) continue
    console.log(`Recording ${name}`)
    const host = new Host(name === 'setup')
    host.start()
    const clip = new Clip(host)
    try {
      await run(clip, page)
      if (clip.frames.length) await render(page, clip.frames, name)
    } finally {
      host.stop()
    }
  }
} finally {
  await browser.close()
}
