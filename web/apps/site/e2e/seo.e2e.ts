import { expect, test } from '@playwright/test'
import { indexablePaths } from '../content/seo-plugin'
import { docManifest } from '../content/manifest'

// Link-preview services must get complete metadata without running JavaScript.
test.describe('crawler responses', () => {
  test.use({ javaScriptEnabled: false })

  test('sitemap lists canonical pages with complete server-rendered metadata', async ({ request, page }, info) => {
    test.skip(info.project.name !== 'desktop', 'The raw HTTP responses do not depend on the viewport')
    test.setTimeout(120_000)
    const robots = await request.get('/robots.txt')
    expect(robots.status()).toBe(200)
    expect(robots.headers()['content-type']).toContain('text/plain')
    expect(await robots.text()).toBe('User-agent: *\nAllow: /\n\nSitemap: https://vibeke.dev/sitemap.xml\n')
    const sitemap = await request.get('/sitemap.xml')
    expect(sitemap.status()).toBe(200)
    expect(sitemap.headers()['content-type']).toMatch(/(?:application|text)\/xml/)
    const xml = await sitemap.text()
    expect(xml).toContain('xmlns="http://www.sitemaps.org/schemas/sitemap/0.9"')
    const urls = [...xml.matchAll(/<loc>([^<]+)<\/loc>/g)].map(match => match[1]!)
    expect(urls).toEqual(indexablePaths.map(path => `https://vibeke.dev${path}`))
    expect(new Set(urls).size).toBe(urls.length)
    for (const path of indexablePaths) {
      const response = await request.get(path, { maxRedirects: 0 })
      expect(response.status(), path).toBe(200)
      expect(response.headers()['x-robots-tag'] ?? '', path).not.toContain('noindex')
      const head = (await response.text()).match(/<head>([\s\S]*?)<\/head>/)?.[1]
      expect(head, path).toBeTruthy()
      await page.setContent(`<html><head>${head}</head><body></body></html>`)
      const canonical = page.locator('link[rel="canonical"]')
      await expect(canonical).toHaveCount(1)
      await expect(canonical).toHaveAttribute('href', `https://vibeke.dev${path}`)
      const title = await page.title()
      const description = await page.locator('meta[name="description"]').getAttribute('content')
      expect(title.length).toBeGreaterThan(5)
      expect(description!.length).toBeGreaterThan(10)
      const doc = docManifest.find(doc => path === `/docs/${doc.slug}`)
      if (doc) expect(title).toBe(`${doc.title} — Vibeke Docs`)
      for (const [attribute, name, content] of [
        ['property', 'og:title', title], ['property', 'og:description', description],
        ['property', 'og:url', `https://vibeke.dev${path}`],
        ['property', 'og:image', 'https://vibeke.dev/brand/workflow-social.png'],
        ['name', 'twitter:card', 'summary_large_image'],
        ['name', 'twitter:title', title], ['name', 'twitter:description', description],
      ]) {
        const meta = page.locator(`meta[${attribute}="${name}"]`)
        await expect(meta).toHaveCount(1)
        await expect(meta).toHaveAttribute('content', content!)
      }
      await expect(page.locator('meta[name="robots"]')).toHaveAttribute('content', /^index, follow/)
      const data = JSON.parse((await page.locator('script[type="application/ld+json"]').textContent())!)
      expect(data['@context']).toBe('https://schema.org')
      expect(data['@graph'][0].url).toBe(`https://vibeke.dev${path}`)
      if (path !== '/') {
        const items = data['@graph'].find((node: { '@type': string }) => node['@type'] === 'BreadcrumbList').itemListElement
        expect(items.at(-1).item).toBe(`https://vibeke.dev${path}`)
        expect(items.map((item: { position: number }) => item.position)).toEqual(items.map((_: unknown, i: number) => i + 1))
      }
    }
  })

  test('social image is a public 1200 by 630 PNG', async ({ request }) => {
    const response = await request.get('/brand/workflow-social.png')
    expect(response.status()).toBe(200)
    expect(response.headers()['content-type']).toContain('image/png')
    const image = await response.body()
    expect(image.subarray(0, 8).toString('hex')).toBe('89504e470d0a1a0a')
    expect(image.readUInt32BE(16)).toBe(1200)
    expect(image.readUInt32BE(20)).toBe(630)
  })

  test('query strings and trailing slashes use clean canonical URLs', async ({ page }) => {
    for (const path of ['/docs/?utm_source=test', '/docs/install/?utm_source=test', '/?utm_source=test']) {
      await page.goto(path)
      const canonical = `https://vibeke.dev${path.split('?')[0]!.replace(/\/+$/, '') || '/'}`
      await expect(page.locator('link[rel="canonical"]')).toHaveAttribute('href', canonical)
      await expect(page.locator('meta[property="og:url"]')).toHaveAttribute('content', canonical)
    }
  })

  test('unknown routes return 404 and cannot be indexed', async ({ page }) => {
    for (const path of ['/missing-seo-test-page', '/docs/missing-seo-test-chapter']) {
      const response = await page.goto(path)
      expect(response?.status()).toBe(404)
      await expect(page.locator('meta[name="robots"]')).toHaveAttribute('content', 'noindex')
      await expect(page.locator('link[rel="canonical"]')).toHaveCount(0)
    }
  })
})

test('client navigation updates the canonical URL and social metadata', async ({ page }) => {
  await page.goto('/')
  await page.getByRole('link', { name: 'Installation requirements and verification' }).click()
  await expect(page).toHaveURL('/docs/install')
  await expect(page.locator('link[rel="canonical"]')).toHaveCount(1)
  await expect(page.locator('link[rel="canonical"]')).toHaveAttribute('href', 'https://vibeke.dev/docs/install')
  await expect(page.locator('meta[property="og:title"]')).toHaveAttribute('content', 'Installation — Vibeke Docs')
  await expect(page.locator('meta[property="og:url"]')).toHaveAttribute('content', 'https://vibeke.dev/docs/install')
})
