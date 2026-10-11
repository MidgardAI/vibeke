import { describe, expect, test } from 'bun:test'
import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { docManifest } from '../content/manifest'

const summary = readFileSync(fileURLToPath(new URL('../../../../docs/site/src/SUMMARY.md', import.meta.url)), 'utf8')

interface Entry { group: string; title: string; file: string }

// The book's SUMMARY.md lists the chapters. Its first, ungrouped chapter belongs to the first group of the site.
function parseSummary(): Entry[] {
  const entries: Entry[] = []
  let group = ''
  for (const line of summary.split('\n')) {
    const heading = /^# (.+)$/.exec(line)
    if (heading && heading[1] !== 'Summary') {
      group = heading[1]!
      continue
    }
    const prefix = /^\[([^\]]+)\]\(([^)]+)\)$/.exec(line)
    if (prefix) entries.push({ group: docManifest[0].group, title: prefix[1]!, file: prefix[2]! })
    const item = /^- \[([^\]]+)\]\(([^)]+)\)$/.exec(line)
    if (item) entries.push({ group, title: item[1]!, file: item[2]! })
  }
  return entries
}

describe('documentation summary and site manifest', () => {
  test('list the same chapters with the same titles, groups and order', () => {
    const fromBook = parseSummary()
    const fromSite = docManifest.map(doc => ({ group: doc.group, title: doc.title, file: doc.file }))
    expect(fromBook).toEqual(fromSite)
  })
})
