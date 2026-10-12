import type { Plugin } from 'vite'
import { docManifest } from './manifest.ts'
import { canonicalUrl, siteOrigin } from '../src/lib/seo.ts'

export const indexablePaths = ['/', '/compare', '/docs', ...docManifest.map(doc => `/docs/${doc.slug}`)]

// Use the same manifest as the docs routes. Do not invent modification dates at build time.
export function sitemapXml() {
  const escape = (value: string) => value.replace(/&/g, '&amp;').replace(/</g, '&lt;').replace(/>/g, '&gt;').replace(/"/g, '&quot;').replace(/'/g, '&apos;')
  return `<?xml version="1.0" encoding="UTF-8"?>\n<urlset xmlns="http://www.sitemaps.org/schemas/sitemap/0.9">\n${indexablePaths.map(path => `  <url><loc>${escape(canonicalUrl(path))}</loc></url>`).join('\n')}\n</urlset>\n`
}

export const robotsTxt = `User-agent: *\nAllow: /\n\nSitemap: ${siteOrigin}/sitemap.xml\n`

export function seoPlugin(): Plugin {
  const files = () => [
    { fileName: 'sitemap.xml', source: sitemapXml(), contentType: 'application/xml; charset=utf-8' },
    { fileName: 'robots.txt', source: robotsTxt, contentType: 'text/plain; charset=utf-8' },
  ]
  return {
    name: 'vibeke-seo',
    generateBundle() {
      if (this.environment.name !== 'client') return
      for (const { fileName, source } of files()) this.emitFile({ type: 'asset', fileName, source })
    },
    configureServer(server) {
      server.middlewares.use((req, res, next) => {
        const file = files().find(file => req.url?.split('?')[0] === `/${file.fileName}`)
        if (!file) return next()
        res.setHeader('Content-Type', file.contentType)
        res.end(file.source)
      })
    },
  }
}
