// Smoke-test the independently deployed PWA, including its offline shell.
// bun scripts/check-app.ts https://app.vibeke.dev
import assert from 'node:assert/strict'
import { chromium, expect } from '@playwright/test'

const origin = process.argv[2] ?? 'https://app.vibeke.dev'
const browser = await chromium.launch(process.platform === 'darwin' ? { channel: 'chrome' } : {})
try {
  for (const viewport of [{ width: 1440, height: 1000 }, { width: 390, height: 844 }]) {
    const context = await browser.newContext({ viewport })
    const page = await context.newPage()
    const errors: string[] = []
    page.on('pageerror', error => errors.push(error.message))
    const response = await page.goto(`${origin}/#/pair`)
    assert.equal(response?.status(), 200)
    const headers = response?.headers() ?? {}
    const csp = headers['content-security-policy'] ?? ''
    for (const directive of ["script-src 'self'", "object-src 'none'", "base-uri 'none'", "frame-ancestors 'none'", "connect-src 'self' wss:"]) {
      assert(csp.includes(directive), `Content-Security-Policy is missing ${directive}: ${csp}`)
    }
    assert.equal(headers['x-content-type-options'], 'nosniff')
    assert.equal(headers['referrer-policy'], 'no-referrer')
    assert(headers['permissions-policy'], 'Permissions-Policy header missing')
    await expect(page.getByRole('heading', { name: 'Pair with a host', exact: true })).toBeVisible()
    const manifest = await page.evaluate(async () => {
      const response = await fetch('/manifest.webmanifest')
      return { status: response.status, body: await response.json() }
    })
    assert.equal(manifest.status, 200)
    assert.equal(manifest.body.start_url, '/#/')
    await page.evaluate(async () => {
      const ready = await navigator.serviceWorker.ready
      if (!ready.active) throw new Error('Service worker not active')
    })
    await expect.poll(() => page.evaluate(() => Boolean(navigator.serviceWorker.controller))).toBe(true)
    await context.setOffline(true)
    await page.reload()
    await expect(page.getByRole('heading', { name: 'Pair with a host', exact: true })).toBeVisible()
    assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'Horizontal overflow')
    assert.deepEqual(errors, [])
    await context.close()
    console.log(`PASS ${origin}: ${viewport.width}px app, pairing route, manifest, service worker, offline reload`)
  }
} finally {
  await browser.close()
}
