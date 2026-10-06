import { describe, expect, test } from 'bun:test'
import { loadDocs } from '../content/docs-plugin'
import { docManifest } from '../content/manifest'
import { searchDocs } from '../src/lib/search'

const docs = loadDocs()

describe('canonical documentation pipeline', () => {
  test('every chapter resolves includes and every internal chapter link has a route', () => {
    const slugs = new Set(docs.map(doc => doc.slug))
    expect(slugs.size).toBe(docManifest.length)
    for (const doc of docs) {
      expect(doc.body.length).toBeGreaterThan(100)
      expect(doc.body).not.toContain('{{#include')
      expect(doc.body).not.toMatch(/^# /m)
      for (const match of doc.body.matchAll(/\]\(\/docs\/([^#)]+)(?:#[^)]*)?\)/g)) {
        expect(slugs.has(match[1]!)).toBe(true)
      }
    }
  })

  test('included release and migration chapters retain their content and source links', () => {
    expect(docs.find(doc => doc.slug === 'releases')?.body).toContain('VIBEKE_ALLOW_UNSIGNED=1')
    expect(docs.find(doc => doc.slug === 'releases')?.body).toContain('https://github.com/MidgardAI/vibeke/blob/main/docs/hardening.md')
    expect(docs.find(doc => doc.slug === 'migrating-from-herdr')?.body).toContain('vibeke import herdr --dry-run')
    expect(docs.find(doc => doc.slug === 'cli')?.body).toContain('](/docs/api)')
  })

  test('code comments do not leak into the table of contents', () => {
    const config = docs.find(doc => doc.slug === 'config')!
    expect(config.headings.some(heading => heading.text.includes('Built-ins'))).toBe(false)
    for (const doc of docs) expect(new Set(doc.headings.map(heading => heading.id)).size).toBe(doc.headings.length)
  })
})

describe('documentation search', () => {
  test('finds content inside included chapters and generated references', () => {
    expect(searchDocs(docs, 'unsigned').map(doc => doc.slug)).toContain('releases')
    expect(searchDocs(docs, 'agent.start').map(doc => doc.slug)).toContain('api')
  })
  test('ranks titles first, matches all words, and handles no results', () => {
    expect(searchDocs(docs, 'configuration')[0]?.slug).toBe('config')
    expect(searchDocs(docs, 'holder process').some(doc => doc.slug === 'holders')).toBe(true)
    expect(searchDocs(docs, 'zzzz-missing-document-zzz')).toEqual([])
    expect(searchDocs(docs, '   ').length).toBeGreaterThan(0)
  })
})

describe('API reference content', () => {
  test('keeps method signatures out of Markdown table cells', () => {
    const api = docs.find(doc => doc.slug === 'api')!
    expect(api.body).toContain('vibeke api call server.status')
    expect(api.body).not.toContain('Holder protocol')
    expect(api.body).not.toContain('VIBEKE_UPDATE_API_FREEZE')
    const rows = api.body.split('\n').filter(line => line.startsWith('|'))
    expect(rows.length).toBeGreaterThan(200)
    for (const row of rows) expect(row.split('|')).toHaveLength(5)
    expect(api.body).toContain('](/api-reference/vibeke-1.schema.json)')
  })
})
