// Shared code for the Herdr plugin corpus scripts (spec 18 §6, §8.5).
// Selection, TOML writing and hashing live here so that `bun test` can cover them.
import { createHash } from 'node:crypto'

export const BASELINE = '0.9.3'
export const CATALOG_URL = 'https://assets.herdr.dev/plugins/index.json'
export const DEFAULT_SIZE = 20

/**
 * Toolchains chosen by hand (checked 2026-10-11 against the official release lists): the current
 * Node 22 LTS, Go 1.26 and Bun 1.4 point releases. Plugins need Go >= 1.26.5 and Bun >= 1.4, so the
 * plugin Bun is newer than the Bun in mise.toml.
 */
export const NODE_VERSION = '22.23.3'
export const GO_VERSION = '1.26.9'
export const BUN_VERSION = '1.4.3'

export interface CatalogManifest {
  path: string
  id?: string
  platforms?: string[]
}
export interface CatalogPlugin {
  id: number
  fullName: string
  stars: number
  createdAt: string
  headCommit: string
  manifests: CatalogManifest[]
}
export interface Catalog {
  generatedAt: string
  plugins: CatalogPlugin[]
}
export interface Exclusion {
  id: number
  repository?: string
  reason: string
}
export interface Candidate {
  repo: CatalogPlugin
  manifest: CatalogManifest
}
export interface IndexEntry {
  repository: string
  plugin_id?: string
  source_commit: string
  manifest_path: string
  sha256: string
}
export interface LockedPlugin {
  rank: number
  repository: string
  repo_id: number
  plugin_id: string
  manifest_path: string
  commit: string
  manifest_sha256: string
  stars: number
  created_at: string
  platforms: string[]
  dependency_mode: 'unpinned' | 'verified'
}
export interface LockData {
  catalog: string
  catalog_sha256: string
  catalog_generated_at: string
  size: number
  toolchains: Record<string, string>
  exclude: Exclusion[]
  plugins: LockedPlugin[]
}

export function sha256Hex(data: string | Uint8Array): string {
  return createHash('sha256').update(data).digest('hex')
}

/** A manifest without `platforms` (or with an empty list) counts as all platforms. */
export function isEligible(m: CatalogManifest): boolean {
  const p = m.platforms
  if (!p || p.length === 0) return true
  return p.includes('macos') || p.includes('linux')
}

/** Stars descending, then older `createdAt`, then repository full name, then manifest path. */
export function compareCandidates(a: Candidate, b: Candidate): number {
  if (a.repo.stars !== b.repo.stars) return b.repo.stars - a.repo.stars
  if (a.repo.createdAt !== b.repo.createdAt) return a.repo.createdAt < b.repo.createdAt ? -1 : 1
  if (a.repo.fullName !== b.repo.fullName) return a.repo.fullName < b.repo.fullName ? -1 : 1
  return a.manifest.path < b.manifest.path ? -1 : a.manifest.path > b.manifest.path ? 1 : 0
}

/** All eligible, non-excluded manifests in rank order (spec 18 §6). */
export function rankCandidates(catalog: Catalog, exclude: Exclusion[]): Candidate[] {
  const skip = new Set(exclude.map(e => e.id))
  const out: Candidate[] = []
  for (const repo of catalog.plugins) {
    if (skip.has(repo.id)) continue
    for (const manifest of repo.manifests ?? []) {
      if (isEligible(manifest)) out.push({ repo, manifest })
    }
  }
  return out.sort(compareCandidates)
}

/** How many ranked candidates the checked-in snapshot keeps: enough to backfill exclusions. */
export const SNAPSHOT_KEEP = 40

/**
 * The checked-in snapshot (spec 18 §6): the first `keep` eligible, non-excluded manifests with
 * only the fields selection needs, plus the source catalog's time and sha256. Excluded
 * repositories and all other entries are left out, which keeps the snapshot small.
 */
export function reduceCatalog(catalog: Catalog, exclude: Exclusion[], sourceSha256: string, keep = SNAPSHOT_KEEP): Catalog & { source: { url: string; sha256: string } } {
  const byRepo = new Map<number, CatalogPlugin>()
  for (const c of rankCandidates(catalog, exclude).slice(0, keep)) {
    let r = byRepo.get(c.repo.id)
    if (!r) {
      r = { id: c.repo.id, fullName: c.repo.fullName, stars: c.repo.stars, createdAt: c.repo.createdAt, headCommit: c.repo.headCommit, manifests: [] }
      byRepo.set(c.repo.id, r)
    }
    const m: CatalogManifest = { path: c.manifest.path, id: c.manifest.id }
    if (c.manifest.platforms) m.platforms = c.manifest.platforms
    r.manifests.push(m)
  }
  return { generatedAt: catalog.generatedAt, source: { url: CATALOG_URL, sha256: sourceSha256 }, plugins: [...byRepo.values()] }
}

export function selectCandidates(catalog: Catalog, exclude: Exclusion[], size = DEFAULT_SIZE): Candidate[] {
  return rankCandidates(catalog, exclude).slice(0, size)
}

export interface Pin {
  commit: string
  manifest_sha256: string
  plugin_id: string
  reused: boolean
}

/**
 * The commit and manifest hash for a candidate. A pin in `manifests/index.json` is reused when it
 * is for the same repository and manifest path and equals the catalog head commit. Otherwise the
 * manifest is downloaded at the head commit through `download`.
 */
export async function resolvePin(
  c: Candidate,
  index: IndexEntry[],
  download: (repository: string, commit: string, path: string) => Promise<string | Uint8Array>,
  pluginIdOf: (manifestText: string) => string | undefined,
): Promise<Pin> {
  const head = c.repo.headCommit
  const known = index.find(
    e => e.repository === c.repo.fullName && e.manifest_path === c.manifest.path && e.source_commit === head,
  )
  if (known) {
    return { commit: head, manifest_sha256: known.sha256, plugin_id: known.plugin_id ?? c.manifest.id ?? '', reused: true }
  }
  const body = await download(c.repo.fullName, head, c.manifest.path)
  const text = typeof body === 'string' ? body : new TextDecoder().decode(body)
  const plugin_id = pluginIdOf(text) ?? c.manifest.id ?? ''
  return { commit: head, manifest_sha256: sha256Hex(body), plugin_id, reused: false }
}

export function lockedFrom(c: Candidate, rank: number, pin: Pin): LockedPlugin {
  return {
    rank,
    repository: c.repo.fullName,
    repo_id: c.repo.id,
    plugin_id: pin.plugin_id,
    manifest_path: c.manifest.path,
    commit: pin.commit,
    manifest_sha256: pin.manifest_sha256,
    stars: c.repo.stars,
    created_at: c.repo.createdAt,
    platforms: [...(c.manifest.platforms ?? [])].sort(),
    dependency_mode: 'unpinned',
  }
}

function q(s: string): string {
  return JSON.stringify(s)
}
function arr(a: string[]): string {
  return `[${a.map(q).join(', ')}]`
}

export const LOCK_HEADER = `# Generated by scripts/herdr-plugins-lock.ts (spec 18 §6). Do not edit by hand.
# Regenerate with: bun scripts/herdr-plugins-lock.ts
# Refresh the catalog snapshot first (a person does this): bun scripts/herdr-plugins-lock.ts --snapshot
# Moving a pin is a reviewed change: read the diff summary the script prints.
# Toolchains: rust comes from mise.toml. Node is the current Node 22 LTS point release (22.23.3),
# Go is Go 1.26.9 and Bun is Bun 1.4.3. These were chosen by hand and checked against the official
# release lists (go.dev/dl and the oven-sh/bun releases) on 2026-10-11.
`

/** Deterministic lock text: fixed key order, no timestamps. */
export function renderLock(l: LockData): string {
  const o: string[] = [LOCK_HEADER]
  o.push('schema = 1', `baseline = ${q(BASELINE)}`, `catalog = ${q(l.catalog)}`)
  o.push(`catalog_sha256 = ${q(l.catalog_sha256)}`, `catalog_generated_at = ${q(l.catalog_generated_at)}`)
  o.push(`size = ${l.size}`, '', '[toolchains]')
  for (const k of ['node', 'bun', 'go', 'rust']) if (l.toolchains[k]) o.push(`${k} = ${q(l.toolchains[k])}`)
  for (const k of Object.keys(l.toolchains).sort()) {
    if (!['node', 'bun', 'go', 'rust'].includes(k)) o.push(`${k} = ${q(l.toolchains[k])}`)
  }
  for (const e of [...l.exclude].sort((a, b) => a.id - b.id)) {
    o.push('', '[[exclude]]', `id = ${e.id}`)
    if (e.repository) o.push(`repository = ${q(e.repository)}`)
    o.push(`reason = ${q(e.reason)}`)
  }
  for (const p of l.plugins) {
    o.push('', '[[plugin]]', `rank = ${p.rank}`, `repository = ${q(p.repository)}`, `repo_id = ${p.repo_id}`)
    o.push(`plugin_id = ${q(p.plugin_id)}`, `manifest_path = ${q(p.manifest_path)}`, `commit = ${q(p.commit)}`)
    o.push(`manifest_sha256 = ${q(p.manifest_sha256)}`, `stars = ${p.stars}`, `created_at = ${q(p.created_at)}`)
    o.push(`platforms = ${arr(p.platforms)}`, `dependency_mode = ${q(p.dependency_mode)}`)
  }
  return o.join('\n') + '\n'
}

/** A readable summary of what changed between two locks. */
export function diffLocks(before: LockedPlugin[], after: LockedPlugin[]): string[] {
  const key = (p: LockedPlugin) => `${p.repository}\u0000${p.manifest_path}`
  const b = new Map(before.map(p => [key(p), p]))
  const a = new Map(after.map(p => [key(p), p]))
  const lines: string[] = []
  for (const [k, p] of a) {
    const old = b.get(k)
    if (!old) lines.push(`new entrant: ${p.repository} ${p.manifest_path} at ${p.commit.slice(0, 12)} (rank ${p.rank})`)
    else if (old.commit !== p.commit) {
      lines.push(`moved pin: ${p.repository} ${p.manifest_path} ${old.commit.slice(0, 12)} -> ${p.commit.slice(0, 12)}`)
    } else if (old.manifest_sha256 !== p.manifest_sha256) {
      lines.push(`changed manifest hash: ${p.repository} ${p.manifest_path}`)
    }
  }
  for (const [k, p] of b) if (!a.has(k)) lines.push(`dropped: ${p.repository} ${p.manifest_path}`)
  return lines
}

/** Markdown for `--check`: new entrants and new head commits compared with the lock. */
export function driftReport(live: Catalog, lock: LockData): string {
  const ranked = selectCandidates(live, lock.exclude, lock.size)
  const locked = new Map(lock.plugins.map(p => [`${p.repository}\u0000${p.manifest_path}`, p]))
  const entrants = ranked.filter(c => !locked.has(`${c.repo.fullName}\u0000${c.manifest.path}`))
  const liveKeys = new Set(ranked.map(c => `${c.repo.fullName}\u0000${c.manifest.path}`))
  const left = lock.plugins.filter(p => !liveKeys.has(`${p.repository}\u0000${p.manifest_path}`))
  const byName = new Map(live.plugins.map(r => [r.fullName, r]))
  const moved = lock.plugins.filter(p => {
    const r = byName.get(p.repository)
    return r && r.headCommit !== p.commit
  })
  const o = ['# Herdr plugin catalog drift', '', `Live catalog generated at ${live.generatedAt}. Lock snapshot generated at ${lock.catalog_generated_at}.`, '']
  o.push('## New entrants', '')
  if (entrants.length === 0) o.push('None.')
  for (const c of entrants) o.push(`- ${c.repo.fullName} \`${c.manifest.path}\` (${c.repo.stars} stars, head ${c.repo.headCommit.slice(0, 12)})`)
  o.push('', '## Locked plugins that left the first ' + lock.size, '')
  if (left.length === 0) o.push('None.')
  for (const p of left) o.push(`- ${p.repository} \`${p.manifest_path}\``)
  o.push('', '## New head commits', '')
  if (moved.length === 0) o.push('None.')
  for (const p of moved) o.push(`- ${p.repository}: ${p.commit.slice(0, 12)} -> ${byName.get(p.repository)!.headCommit.slice(0, 12)}`)
  return o.join('\n') + '\n'
}

export function parseToml(text: string): any {
  return (globalThis as any).Bun.TOML.parse(text)
}

export function lockFromToml(text: string): LockData {
  const t = parseToml(text)
  return {
    catalog: t.catalog,
    catalog_sha256: t.catalog_sha256,
    catalog_generated_at: t.catalog_generated_at,
    size: t.size,
    toolchains: t.toolchains ?? {},
    exclude: t.exclude ?? [],
    plugins: (t.plugin ?? []).map((p: any) => ({ ...p, platforms: p.platforms ?? [] })),
  }
}

export function manifestPluginId(text: string): string | undefined {
  try {
    const t = parseToml(text)
    return typeof t.id === 'string' ? t.id : undefined
  } catch {
    return undefined
  }
}
