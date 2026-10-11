import { describe, expect, test } from 'bun:test'
import {
  type Catalog, type CatalogPlugin, type IndexEntry, type LockData,
  isEligible, rankCandidates, renderLock, resolvePin, selectCandidates, sha256Hex, lockedFrom, lockFromToml, diffLocks,
} from './lib'

const repo = (id: number, name: string, stars: number, createdAt: string, platforms?: string[][], head = 'a'.repeat(40)): CatalogPlugin => ({
  id, fullName: name, stars, createdAt, headCommit: head,
  manifests: (platforms ?? [undefined as any]).map((p, i) => ({ path: `m${i}/herdr-plugin.toml`, id: `${name}.${i}`, ...(p ? { platforms: p } : {}) })),
})
const cat = (plugins: CatalogPlugin[]): Catalog => ({ generatedAt: 'x', plugins })

describe('eligibility', () => {
  test('platforms', () => {
    expect(isEligible({ path: 'p' })).toBe(true)
    expect(isEligible({ path: 'p', platforms: [] })).toBe(true)
    expect(isEligible({ path: 'p', platforms: ['windows'] })).toBe(false)
    expect(isEligible({ path: 'p', platforms: ['windows', 'linux'] })).toBe(true)
  })
  test('windows-only manifests are skipped', () => {
    const r = rankCandidates(cat([repo(1, 'a/w', 9, '2026-01-01', [['windows']]), repo(2, 'a/l', 1, '2026-01-01', [['linux']])]), [])
    expect(r.map(c => c.repo.fullName)).toEqual(['a/l'])
  })
})

describe('ordering', () => {
  test('multi-manifest repos add each eligible manifest and share stars', () => {
    const r = rankCandidates(cat([repo(1, 'a/multi', 5, '2026-01-01', [['linux'], ['windows'], ['macos']]), repo(2, 'b/one', 4, '2026-01-01')]), [])
    expect(r.map(c => `${c.repo.fullName}:${c.manifest.path}`)).toEqual(['a/multi:m0/herdr-plugin.toml', 'a/multi:m2/herdr-plugin.toml', 'b/one:m0/herdr-plugin.toml'])
  })
  test('ties: older createdAt, then name', () => {
    const r = rankCandidates(cat([
      repo(1, 'z/new', 5, '2026-03-01'), repo(2, 'b/same', 5, '2026-02-01'), repo(3, 'a/same', 5, '2026-02-01'), repo(4, 'q/old', 5, '2026-01-01'),
    ]), [])
    expect(r.map(c => c.repo.fullName)).toEqual(['q/old', 'a/same', 'b/same', 'z/new'])
  })
})

describe('exclusion backfill', () => {
  test('next eligible fills the slot', () => {
    const c = cat([repo(1, 'a/one', 9, '2026-01-01'), repo(2, 'a/two', 8, '2026-01-01'), repo(3, 'a/three', 7, '2026-01-01')])
    expect(selectCandidates(c, [], 2).map(x => x.repo.id)).toEqual([1, 2])
    expect(selectCandidates(c, [{ id: 1, reason: 'r' }], 2).map(x => x.repo.id)).toEqual([2, 3])
  })
})

describe('pins', () => {
  const c = selectCandidates(cat([repo(1, 'a/one', 9, '2026-01-01', undefined, 'b'.repeat(40))]), [])[0]
  const idx = (commit: string): IndexEntry[] => [{ repository: 'a/one', source_commit: commit, manifest_path: 'm0/herdr-plugin.toml', sha256: 'f'.repeat(64) }]
  test('reuse when the commit equals the head', async () => {
    const pin = await resolvePin(c, idx('b'.repeat(40)), async () => { throw new Error('no download') }, () => undefined)
    expect(pin.reused).toBe(true)
    expect(pin.manifest_sha256).toBe('f'.repeat(64))
  })
  test('download at the head when the pin is older', async () => {
    let asked = ''
    const pin = await resolvePin(c, idx('c'.repeat(40)), async (_r, commit) => { asked = commit; return 'id = "x.y"\n' }, t => (t.includes('x.y') ? 'x.y' : undefined))
    expect(asked).toBe('b'.repeat(40))
    expect(pin).toMatchObject({ reused: false, plugin_id: 'x.y', manifest_sha256: sha256Hex('id = "x.y"\n') })
  })
})

describe('lock output', () => {
  const c = selectCandidates(cat([repo(1, 'a/one', 9, '2026-01-01', [['macos', 'linux']])]), [])[0]
  const lock: LockData = {
    catalog: 'catalog.json', catalog_sha256: '0'.repeat(64), catalog_generated_at: 't', size: 20,
    toolchains: { rust: '1.99.0', node: '22.0.0', go: '1.25.1', bun: '1.3.0' },
    exclude: [{ id: 7, reason: 'A "quoted" reason.' }],
    plugins: [lockedFrom(c, 1, { commit: 'b'.repeat(40), manifest_sha256: 'e'.repeat(64), plugin_id: 'a.one', reused: false })],
  }
  test('deterministic and parseable', () => {
    const t = renderLock(lock)
    expect(renderLock(lock)).toBe(t)
    expect(t).toContain('platforms = ["linux", "macos"]')
    const back = lockFromToml(t)
    expect(back.plugins).toEqual(lock.plugins)
    expect(back.exclude).toEqual(lock.exclude)
    expect(back.toolchains.node).toBe('22.0.0')
  })
  test('diff summary', () => {
    const moved = { ...lock.plugins[0], commit: 'd'.repeat(40) }
    expect(diffLocks(lock.plugins, [moved])[0]).toContain('moved pin')
    expect(diffLocks([], lock.plugins)[0]).toContain('new entrant')
  })
})
