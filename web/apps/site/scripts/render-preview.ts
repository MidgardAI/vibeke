// Render the narrated home page preview from scripts/capture-preview.ts captures and the
// narration clips (one WAV per segment in scripts/preview-narration.json, named <id>.wav).
// Each frame is laid out in Chrome and piped to ffmpeg. Requires ffmpeg and installed Chrome.
// Run from this directory:
//   bun scripts/render-preview.ts [--capture=/tmp/vibeke-preview-capture] [--audio=/tmp/vibeke-narration] [--only=desktop|mobile] [--frames=1.0,12.4]
// --frames writes single PNG frames to the capture directory instead of videos.
import { execFileSync, spawn } from 'node:child_process'
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'
import { chromium, type Page } from '@playwright/test'

const scriptDir = dirname(fileURLToPath(import.meta.url))
const site = resolve(scriptDir, '..')
const arg = (name: string) => process.argv.find(a => a.startsWith(`--${name}=`))?.slice(name.length + 3)
const captureDir = resolve(arg('capture') ?? '/tmp/vibeke-preview-capture')
const audioDir = resolve(arg('audio') ?? '/tmp/vibeke-narration')
const only = arg('only')
const stills = arg('frames')?.split(',').map(Number)
const FPS = 30
// Versioned names, so browsers and CDNs never mix the new cut with cached old files.
const NAME = 'preview-v2'
const videoDir = join(site, 'public/video')

interface Cell { text: string; fg?: string; bg?: string; bold?: boolean; dim?: boolean; italic?: boolean; underline?: boolean }
interface Capture { marks: Record<string, number>; tui: { t: number; cells: Cell[][] }[]; phone: { t: number; file: string }[]; tap: { x: number; y: number }; phoneViewport: number[] }
const capture: Capture = JSON.parse(readFileSync(join(captureDir, 'capture.json'), 'utf8'))
const narration: { segments: { id: string; gap: number; text: string }[]; tail: number } = JSON.parse(readFileSync(join(scriptDir, 'preview-narration.json'), 'utf8'))

// --- timeline -------------------------------------------------------------------------------------

const seconds = (file: string) => Number(execFileSync('ffprobe', ['-v', 'error', '-show_entries', 'format=duration', '-of', 'csv=p=0', file], { encoding: 'utf8' }))
let clock = 0
const segments = narration.segments.map(s => {
  const file = join(audioDir, `${s.id}.wav`)
  if (!stills && !existsSync(file)) throw new Error(`Missing narration clip: ${file}. Supply every WAV before rendering videos.`)
  const length = existsSync(file) ? seconds(file) : s.text.split(' ').length * 0.4 + 0.5
  const start = clock + s.gap
  clock = start + length
  return { ...s, file: existsSync(file) ? file : undefined, start, end: clock }
})
const DURATION = Math.ceil((clock + narration.tail) * FPS) / FPS
const [s1, s2, s3, s4, s5] = segments as [typeof segments[0], typeof segments[0], typeof segments[0], typeof segments[0], typeof segments[0]]
const cut = [0, s2.start - 0.2, s3.start - 0.25, s4.start - 0.25, s5.start - 0.2, DURATION]
const tapAt = s3.start + 1.5 // on "Approve it"
const m = capture.marks
const timeline = {
  duration: DURATION, cut, tapAt,
  // Scene time → capture time (ms). Scene 1 starts once the agent header is on screen and stops
  // before the workspace switch; scene 2 lands the request on "needs permission".
  tui1: { from: cut[0]!, at: firstScreen(t => t >= m.agent!, 'Claude Code'), rate: 800, until: m.away! - 50 },
  tui2: { from: cut[1]!, at: m.request! - 140 - (s2.start + 0.6 - cut[1]!) * 1000, rate: 1000, until: m.phoneBefore! },
  // Scene 4: back in the agent's workspace, while the TUI still shows who answered.
  tui4: { from: cut[3]!, at: m.back! + 250, rate: 1000, until: m.end! },
  zoomAt: s2.start + 0.6 + 0.4,
}
function firstScreen(when: (t: number) => boolean, text: string) {
  return capture.tui.find(f => when(f.t) && f.cells.some(l => l.map(c => c.text).join('').includes(text)))!.t
}
// The request reached the TUI a moment before the interaction list reported it.
const requestScreen = capture.tui.find(f => f.cells.some(l => l.map(c => c.text).join('').includes('needs you')))!.t
timeline.tui2.at = requestScreen - (s2.start + 0.6 - cut[1]!) * 1000

// --- page -----------------------------------------------------------------------------------------

const esc = (s: string) => s.replaceAll('&', '&amp;').replaceAll('<', '&lt;').replaceAll('>', '&gt;')
// Transient key-mode indicators (PREFIX, NAV) are skipped: they only show the keys pressed.
const screens = capture.tui.filter(f => !f.cells[0]!.some(c => /PREFIX|NAV/.test(c.text))).map(f => ({
  t: f.t,
  html: f.cells.map(line => '<div>' + (line.map(c => {
    const css = [c.fg && `color:${c.fg}`, c.bg && `background:${c.bg}`, c.bold && 'font-weight:700', c.italic && 'font-style:italic', c.dim && 'opacity:.62', c.underline && 'text-decoration:underline'].filter(Boolean).join(';')
    return `<span style="${css}">${esc(c.text)}</span>`
  }).join('') || ' ') + '</div>').join(''),
}))
const phone = {
  before: 'phone/before.png',
  frames: capture.phone.filter(f => f.t > m.tap!).map(f => ({ t: f.t - m.tap!, file: f.file })),
  tap: capture.tap, width: capture.phoneViewport[0]!,
}

const font = (file: string) => pathToFileURL(join(site, 'node_modules/@fontsource-variable', file)).href
const duck = pathToFileURL(join(site, 'public/brand/duck-256.png')).href

interface Layout {
  name: 'desktop' | 'mobile'; w: number; h: number
  head: { x: number; y: number; size: number; width: number }
  stage: { x: number; y: number; w: number; h: number }
  views: { s1: View; s2a: View; s2b: View; s4: View }
  phoneCrop: number // CSS px of the phone screen shown from the top (header and request card)
  label: number
}
interface View { c0: number; r0: number; cols: number; rows: number }
const layouts: Layout[] = [
  {
    name: 'desktop', w: 1920, h: 1080,
    head: { x: 120, y: 92, size: 66, width: 1680 },
    stage: { x: 120, y: 222, w: 1680, h: 790 },
    views: { s1: { c0: 0, r0: 0, cols: 64, rows: 9 }, s2a: { c0: 0, r0: 0, cols: 64, rows: 11 }, s2b: { c0: 0, r0: 0, cols: 48, rows: 10 }, s4: { c0: 29, r0: 0, cols: 64, rows: 15 } },
    phoneCrop: 262, label: 26,
  },
  {
    name: 'mobile', w: 1080, h: 1080,
    head: { x: 64, y: 70, size: 58, width: 952 },
    stage: { x: 48, y: 236, w: 984, h: 790 },
    views: { s1: { c0: 29, r0: 0, cols: 32, rows: 8 }, s2a: { c0: 0, r0: 0, cols: 40, rows: 10 }, s2b: { c0: 0, r0: 0, cols: 38, rows: 10 }, s4: { c0: 29, r0: 1, cols: 34, rows: 14 } },
    phoneCrop: 262, label: 24,
  },
]

const HEADLINES = [
  'Run your <em>usual agent</em> in your terminal.',
  'When it needs you, <em>the request waits.</em>',
  'Away from your desk? <em>Allow it from your phone.</em>',
  'The agent <em>continues.</em>',
]

function html(l: Layout) {
  return `<!doctype html><html><head><meta charset="utf-8"><style>
@font-face{font-family:Geist;src:url(${font('geist/files/geist-latin-wght-normal.woff2')}) format('woff2');font-weight:100 900}
@font-face{font-family:'Geist Mono';src:url(${font('geist-mono/files/geist-mono-latin-wght-normal.woff2')}) format('woff2');font-weight:100 900}
:root{--term-bg:#1e1e2e;--term-fg:#cdd6f4;--ansi-black:#45475a;--ansi-red:#f38ba8;--ansi-green:#a6e3a1;--ansi-yellow:#f9e2af;--ansi-blue:#89b4fa;--ansi-magenta:#cba6f7;--ansi-cyan:#94e2d5;--ansi-white:#cdd6f4;--ansi-bright-black:#585b70;--ansi-bright-white:#ffffff}
*{box-sizing:border-box}html,body{margin:0;width:${l.w}px;height:${l.h}px;overflow:hidden;background:#111311;color:#f0eee7;font-family:Geist,sans-serif;-webkit-font-smoothing:antialiased}
.bg{position:absolute;inset:0;background-image:linear-gradient(#a2be8b07 1px,transparent 1px),linear-gradient(90deg,#a2be8b07 1px,transparent 1px);background-size:48px 48px}
.glow{position:absolute;inset:0;background:radial-gradient(ellipse 70% 60% at 85% 0%,#eeb97812,transparent 70%)}
.scene{position:absolute;inset:0;opacity:0}
h1{position:absolute;left:${l.head.x}px;top:${l.head.y}px;width:${l.head.width}px;margin:0;font-size:${l.head.size}px;line-height:1.08;font-weight:500;letter-spacing:-.045em}
h1 em{font-style:normal;color:#eeb978}
.stage{position:absolute;left:${l.stage.x}px;top:${l.stage.y}px;width:${l.stage.w}px;height:${l.stage.h}px}
.window{position:absolute;left:0;right:0;top:0;border:1px solid #3a4137;border-radius:16px;background:var(--term-bg);overflow:hidden;box-shadow:0 30px 80px #000a}
.bar{height:40px;display:flex;gap:10px;align-items:center;padding:0 18px;background:#181825;border-bottom:1px solid #2b2c3c}
.bar i{width:13px;height:13px;border-radius:50%;background:#45475a}
.view{position:absolute;left:30px;top:62px;right:0;bottom:0;overflow:hidden;-webkit-mask-image:linear-gradient(90deg,#000 calc(100% - 60px),transparent),linear-gradient(#000 calc(100% - 16px),transparent);-webkit-mask-composite:source-in}
.grid{position:absolute;left:0;top:0;transform-origin:0 0;font:20px/27px Menlo,'DejaVu Sans Mono',monospace;color:var(--term-fg);white-space:pre}
.grid div{height:27px}.grid span{display:inline-block;height:27px;vertical-align:top}
.mark{position:absolute;border:3px solid #eeb978;border-radius:10px;opacity:0;box-shadow:0 0 0 9999px #11131100}
.device{position:absolute;left:50%;top:0;border:14px solid #262b25;border-bottom:0;border-radius:64px 64px 0 0;overflow:hidden;background:#0b0b0b;box-shadow:0 30px 80px #000a;-webkit-mask-image:linear-gradient(#000 82%,transparent)}
.device img{position:absolute;left:0;top:0;width:100%;opacity:0}
.tap{position:absolute;width:84px;height:84px;margin:-42px 0 0 -42px;border-radius:50%;border:4px solid #f0eee7;opacity:0}
.end{display:flex;flex-direction:column;align-items:center;justify-content:center;text-align:center}
.end img{width:${l.name === 'desktop' ? 190 : 170}px;height:auto}
.end h2{margin:36px 0 0;font-size:${l.name === 'desktop' ? 84 : 68}px;line-height:1.06;font-weight:500;letter-spacing:-.05em}
.end h2 em{font-style:normal;color:#eeb978}
.end p{margin:40px 0 0;font:500 ${l.name === 'desktop' ? 34 : 32}px 'Geist Mono',monospace;color:#a0a79b}
.label{position:absolute;left:${l.head.x}px;bottom:${l.name === 'desktop' ? 22 : 18}px;font:${l.label}px 'Geist Mono',monospace;color:#7f877b}
</style></head><body><div class="bg"></div><div class="glow"></div>
${[0, 1, 3].map(i => `<section class="scene" id="s${i}"><h1>${HEADLINES[i]}</h1><div class="stage"><div class="window"><div class="bar"><i></i><i></i><i></i></div><div class="view"><div class="grid"></div>${i === 1 ? '<div class="mark"></div>' : ''}</div></div></div></section>`).join('')}
<section class="scene" id="s2"><h1>${HEADLINES[2]}</h1><div class="stage"><div class="device">
<img data-src="${phone.before}">${phone.frames.map(f => `<img data-src="${f.file}">`).join('')}
<div class="tap"></div></div></div></section>
<section class="scene end" id="s4"><img src="${duck}" alt=""><h2>Run agents in your terminal.<br><em>Answer from your phone.</em></h2><p>vibeke.dev</p></section>
<div class="label">Sample workspace · scripted agent output</div>
</body></html>`
}

/** Runs in the page: draw the frame at time `t` (seconds). */
function setup(cfg: { layout: Layout; timeline: typeof timeline; screens: { t: number; html: string }[]; phone: typeof phone }) {
  const { layout: l, timeline: tl, screens, phone } = cfg
  const $ = (s: string, root: ParentNode = document) => root.querySelector(s) as HTMLElement
  const clamp = (x: number) => Math.max(0, Math.min(1, x))
  const ease = (x: number) => { x = clamp(x); return x < 0.5 ? 4 * x * x * x : 1 - (-2 * x + 2) ** 3 / 2 }
  const probe = document.createElement('span'); probe.className = 'grid'; probe.textContent = 'x'.repeat(100); probe.style.visibility = 'hidden'; document.body.append(probe)
  const cw = probe.getBoundingClientRect().width / 100, lh = 27
  probe.remove()
  for (const img of document.querySelectorAll<HTMLImageElement>('img[data-src]')) img.src = img.dataset.src!
  // Phone: the top of the screen, as large as the stage allows.
  const device = $('.device'), screenW = phone.width
  const scale = Math.min((l.stage.w - 28) / screenW, (l.stage.h) / l.phoneCrop)
  device.style.width = `${screenW * scale + 28}px`; device.style.height = `${l.stage.h}px`; device.style.marginLeft = `${-(screenW * scale + 28) / 2}px`
  const tap = $('.tap'); tap.style.left = `${phone.tap.x * scale}px`; tap.style.top = `${phone.tap.y * scale}px`
  const imgs = [...device.querySelectorAll('img')]
  const view = (id: string) => $(`#${id} .view`)
  function camera(id: string, v: { c0: number; r0: number; cols: number; rows: number }, push: number) {
    const el = view(id), win = el.parentElement!
    const s = Math.min((l.stage.w - 60) / (v.cols * cw), (l.stage.h - 62 - 30) / (v.rows * lh)) * push
    const height = Math.min(l.stage.h, 62 + v.rows * lh * s / push + 30)
    win.style.height = `${height}px`; win.style.top = `${(l.stage.h - height) / 2}px`
    el.style.right = 'auto'; el.style.width = `${Math.min(l.stage.w - 30, v.cols * cw * s + 2)}px`
    const grid = $(`#${id} .grid`)
    grid.style.transform = `translate(${-v.c0 * cw * s}px,${-v.r0 * lh * s}px) scale(${s})`
    return { s, x: (c: number) => (c - v.c0) * cw * s, y: (r: number) => (r - v.r0) * lh * s }
  }
  const lerp = (a: typeof l.views.s1, b: typeof l.views.s1, k: number) => ({ c0: a.c0 + (b.c0 - a.c0) * k, r0: a.r0 + (b.r0 - a.r0) * k, cols: a.cols + (b.cols - a.cols) * k, rows: a.rows + (b.rows - a.rows) * k })
  const shown: Record<string, number> = {}
  function screen(id: string, map: { from: number; at: number; rate: number; until: number }, t: number) {
    const ct = Math.min(map.at + (t - map.from) * map.rate, map.until)
    let i = 0
    while (i + 1 < screens.length && screens[i + 1]!.t <= ct) i++
    if (shown[id] !== i) { shown[id] = i; $(`#${id} .grid`).innerHTML = screens[i]!.html }
  }
  const FADE = 0.45
  return (t: number) => {
    const scenes = ['s0', 's1', 's2', 's3', 's4']
    scenes.forEach((id, i) => {
      const a = tl.cut[i]!, b = tl.cut[i + 1]!
      // Let the terminal fade out before the closing card appears.
      const inK = i === 0 ? 1 : ease((t - a) / FADE)
      const outK = i === scenes.length - 1 ? 1 : 1 - ease((t - b + (i === scenes.length - 2 ? FADE : 0)) / FADE)
      const o = Math.min(inK, outK)
      const el = $(`#${id}`)
      el.style.opacity = String(o)
      el.style.visibility = o > 0 ? 'visible' : 'hidden'
      const h = el.querySelector('h1') as HTMLElement | null
      // Headlines hand over without overlapping: the old one leaves first.
      if (h) {
        const k = i === 0 ? ease(t / 0.6) : ease((t - a - 0.15) / 0.45)
        h.style.transform = `translateY(${(1 - k) * 18}px)`
        h.style.opacity = String(Math.min(k, i === scenes.length - 1 ? 1 : 1 - ease((t - b) / 0.2)) / Math.max(o, 0.001))
      }
      if (o <= 0) return
      const local = t - a, span = b - a
      const push = 1 + 0.018 * ease(local / span)
      if (id === 's0') { screen(id, tl.tui1, t); camera(id, l.views.s1, push) }
      if (id === 's1') {
        screen(id, tl.tui2, t)
        const k = ease((t - tl.zoomAt) / 0.9)
        const c = camera(id, lerp(l.views.s2a, l.views.s2b, k), push)
        // Mark the "needs you" rows once the request has arrived.
        const mark = $('#s1 .mark'), pad = 10 * c.s / 2
        mark.style.left = `${c.x(0) - pad}px`; mark.style.top = `${c.y(1) - pad}px`
        mark.style.width = `${c.x(l.views.s2b.cols) - c.x(0) + 2 * pad}px`; mark.style.height = `${c.y(3) - c.y(1) + 2 * pad}px`
        mark.style.opacity = String(ease((t - tl.zoomAt - 0.6) / 0.4))
      }
      if (id === 's2') {
        const d = t - tl.tapAt
        // Delivery over the local relay takes about two seconds. The edit keeps the first half
        // second after the tap, then cuts to the "Delivered" state.
        const pt = d < 0 ? -1 : d < 0.5 ? d * 1000 : d < 0.55 ? 500 : Math.min(1990 + (d - 0.55) * 1000, 2900)
        let show = 0
        phone.frames.forEach((f, j) => { if (pt >= f.t) show = j + 1 })
        imgs.forEach((img, j) => { img.style.opacity = j === show ? '1' : '0' })
        tap.style.opacity = d < -0.35 || d > 0.7 ? '0' : String(d < 0 ? 1 - Math.abs(d) / 0.35 * 0.6 : 1 - d / 0.7)
        tap.style.transform = `scale(${d < 0 ? 1 : 1 + d * 1.1})`
        tap.style.background = d < 0 ? '#f0eee733' : 'transparent'
        device.style.transform = `scale(${1 + 0.012 * ease(local / span)})`; device.style.transformOrigin = '50% 0'
      }
      if (id === 's3') { screen(id, tl.tui4, t); camera(id, l.views.s4, push) }
      if (id === 's4') { const k = ease((t - a) / 0.8); el.style.transform = `translateY(${(1 - k) * 14}px)`; $('.label').style.opacity = String(1 - k) }
    })
  }
}

async function frameAt(page: Page, t: number) {
  await page.evaluate(t => (window as any).draw(t), t)
  return page.screenshot({ type: 'png' })
}

function mixAudio(file: string) {
  const voiced = segments.filter(s => s.file)
  if (!voiced.length) return false
  const inputs = voiced.flatMap(s => ['-i', s.file!])
  const delays = voiced.map((s, i) => `[${i}]aresample=48000,adelay=${Math.round(s.start * 1000)}:all=1[a${i}]`).join(';')
  const mix = `${delays};${voiced.map((_, i) => `[a${i}]`).join('')}amix=inputs=${voiced.length}:normalize=0,apad=whole_dur=${DURATION},atrim=0:${DURATION},loudnorm=I=-16:TP=-1.5:LRA=11,aresample=48000[out]`
  execFileSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', ...inputs, '-filter_complex', mix, '-map', '[out]', '-ac', '1', '-c:a', 'pcm_s16le', file, '-y'])
  return true
}

function vtt() {
  const ts = (s: number) => `${String(Math.floor(s / 60)).padStart(2, '0')}:${(s % 60).toFixed(3).padStart(6, '0')}`
  return 'WEBVTT\n\n' + segments.map(s => `${ts(s.start)} --> ${ts(Math.min(s.end + 0.3, DURATION))}\n${s.text}\n`).join('\n')
}

const browser = await chromium.launch(process.platform === 'darwin' ? { channel: 'chrome' } : {})
try {
  const work = join(captureDir, 'render')
  mkdirSync(work, { recursive: true })
  const audio = join(work, 'narration.wav')
  const voiced = !stills && mixAudio(audio)
  for (const layout of layouts.filter(l => !only || l.name === only)) {
    const page = await browser.newPage({ viewport: { width: layout.w, height: layout.h }, deviceScaleFactor: 1 })
    const htmlFile = join(captureDir, `preview-${layout.name}.html`)
    writeFileSync(htmlFile, html(layout))
    await page.goto(pathToFileURL(htmlFile).href)
    await page.evaluate(`window.draw = (${setup.toString()})(${JSON.stringify({ layout, timeline, screens, phone })})`)
    await page.evaluate(async () => { await document.fonts.ready; await Promise.all([...document.images].map(i => i.decode().catch(() => {}))) })
    if (stills) {
      for (const t of stills) writeFileSync(join(captureDir, `frame-${layout.name}-${t.toFixed(2)}.png`), await frameAt(page, t))
      await page.close(); continue
    }
    const base = join(videoDir, layout.name === 'desktop' ? NAME : `${NAME}-mobile`)
    if (/tour/.test(base)) throw new Error('refusing to overwrite the full tour')
    const silent = join(work, `${layout.name}.mp4`)
    const ff = spawn('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-f', 'image2pipe', '-framerate', String(FPS), '-c:v', 'png', '-i', '-',
      '-c:v', 'libx264', '-preset', 'slow', '-crf', '16', '-pix_fmt', 'yuv420p', '-profile:v', 'high', '-g', String(FPS * 2), silent, '-y'], { stdio: ['pipe', 'inherit', 'inherit'] })
    const frames = Math.round(DURATION * FPS)
    for (let i = 0; i < frames; i++) {
      const png = await frameAt(page, i / FPS)
      if (!ff.stdin.write(png)) await new Promise(r => ff.stdin.once('drain', r))
      if (i % 60 === 0) process.stdout.write(`\r${layout.name} ${i}/${frames}`)
    }
    ff.stdin.end()
    await new Promise((r, j) => ff.on('close', code => code === 0 ? r(null) : j(new Error(`ffmpeg exited ${code}`))))
    writeFileSync(`${base}-poster.png`, await frameAt(page, tapAt - 0.5))
    execFileSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-i', `${base}-poster.png`, '-q:v', '2', `${base}-poster.jpg`, '-y'])
    execFileSync('rm', [`${base}-poster.png`])
    const audioIn = voiced ? ['-i', audio] : []
    execFileSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-i', silent, ...audioIn, '-map', '0:v', ...(voiced ? ['-map', '1:a', '-c:a', 'aac', '-b:a', '160k'] : []), '-c:v', 'copy', '-movflags', '+faststart', '-shortest', `${base}.mp4`, '-y'])
    execFileSync('ffmpeg', ['-hide_banner', '-loglevel', 'error', '-i', silent, ...audioIn, '-map', '0:v', ...(voiced ? ['-map', '1:a', '-c:a', 'libopus', '-b:a', '96k'] : []),
      '-c:v', 'libvpx-vp9', '-crf', '24', '-b:v', '0', '-row-mt', '1', '-deadline', 'good', '-cpu-used', '2', '-g', String(FPS * 2), '-pix_fmt', 'yuv420p', `${base}.webm`, '-y'])
    process.stdout.write(`\r${layout.name}: ${base}.mp4, .webm, -poster.jpg\n`)
    await page.close()
  }
  if (!stills) writeFileSync(join(videoDir, `${NAME}.vtt`), vtt())
  console.log(`Duration ${DURATION.toFixed(2)} s`, segments.map(s => `${s.id} ${s.start.toFixed(2)}–${s.end.toFixed(2)}`).join(', '))
} finally { await browser.close() }
