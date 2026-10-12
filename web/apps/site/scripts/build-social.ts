// Render the share card from committed product captures. Run from web/apps/site.
import { chromium } from '@playwright/test'
import { readFileSync } from 'node:fs'
import { resolve } from 'node:path'
const asset = (path: string) => `data:image/png;base64,${readFileSync(resolve('public', path)).toString('base64')}`
const browser = await chromium.launch({ headless: true, ...(process.platform === 'darwin' ? { channel: 'chrome' } : {}) })
try {
  const page = await browser.newPage({ viewport: { width: 1200, height: 630 }, deviceScaleFactor: 1 })
  await page.setContent(`<!doctype html><html><head><style>
    *{box-sizing:border-box}body{margin:0;background:#111311;color:#f0eee7;font-family:Arial,sans-serif}
    main{height:630px;padding:38px 48px;overflow:hidden;background:radial-gradient(ellipse at 90% 0,#eeb97818,transparent 60%)}
    header{display:flex;align-items:center;gap:12px;font-size:25px;font-weight:700}header img{width:52px;height:52px}
    header span:last-child{margin-left:auto;font:16px monospace;color:#a0a79b}h1{font-size:49px;letter-spacing:-2px;line-height:1.08;margin:18px 0 24px}em{font-style:normal;color:#eeb978}
    .screens{position:relative;height:350px}.terminal{width:850px;border:1px solid #626b57;border-radius:8px;box-shadow:0 10px 30px #0008}
    .phone{position:absolute;right:8px;top:-92px;width:226px;border:5px solid #30362e;border-radius:22px;box-shadow:0 10px 30px #000b}
    .label{position:absolute;bottom:5px;left:20px;background:#111311ed;padding:12px 18px;font:16px monospace;color:#eeb978}
  </style></head><body><main><header><img src="${asset('brand/duck-128.png')}" alt=""/><span>vibeke</span><span>vibeke.dev · Apache-2.0</span></header>
  <h1>Run agents in your terminal.<br/><em>Answer from your phone.</em></h1>
  <div class="screens"><img class="terminal" src="${asset('screenshots/tui.png')}" alt="Terminal workspace"/><img class="phone" src="${asset('screenshots/web.png')}" alt="Phone inbox"/><span class="label">Claude Code · Codex · your usual agent CLI</span></div>
  </main></body></html>`)
  await page.locator('img').evaluateAll(images => Promise.all(images.map(image => image.decode())))
  await page.screenshot({ path: 'public/brand/workflow-social.png' })
} finally { await browser.close() }
