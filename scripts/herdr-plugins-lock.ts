// Select the 20 highest-ranked eligible Herdr plugins and write the lock (spec 18 §6).
//   bun scripts/herdr-plugins-lock.ts             write top20.lock.toml from catalog.json
//   bun scripts/herdr-plugins-lock.ts --snapshot  download the live catalog into catalog.json (a person runs this)
//   bun scripts/herdr-plugins-lock.ts --check     compare the live catalog with the lock and print Markdown; never writes
import { existsSync, mkdirSync, readFileSync, writeFileSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { dirname, join, resolve } from 'node:path'
import {
  BASELINE, CATALOG_URL, DEFAULT_SIZE, GO_VERSION, NODE_VERSION,
  type Catalog, type IndexEntry, type LockData, type LockedPlugin,
  diffLocks, driftReport, lockFromToml, lockedFrom, manifestPluginId, parseToml, renderLock, resolvePin, selectCandidates, sha256Hex,
} from './herdr-plugins/lib'

const root = resolve(import.meta.dir, '..')
const dir = join(root, 'tests/compat/herdr', BASELINE)
const pluginsDir = join(dir, 'plugins')
const catalogPath = join(pluginsDir, 'catalog.json')
const excludePath = join(pluginsDir, 'exclude.toml')
const lockPath = join(pluginsDir, 'top20.lock.toml')
const indexPath = join(dir, 'manifests/index.json')
const cacheDir = process.env.HERDR_PLUGINS_CACHE ?? join(tmpdir(), 'vibeke-herdr-plugins-cache')

const args = process.argv.slice(2)
if (args.includes('--help') || args.includes('-h')) {
  console.log('usage: bun scripts/herdr-plugins-lock.ts [--snapshot | --check]')
  process.exit(0)
}

async function get(url: string): Promise<Uint8Array> {
  const res = await fetch(url)
  if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`)
  return new Uint8Array(await res.arrayBuffer())
}

function loadExclude() {
  return (parseToml(readFileSync(excludePath, 'utf8')).exclude ?? []) as { id: number; reason: string }[]
}

function miseVersion(tool: string): string {
  const text = readFileSync(join(root, 'mise.toml'), 'utf8')
  const m = text.match(new RegExp(`^${tool}\\s*=\\s*(?:"([^"]+)"|\\{[^}]*version\\s*=\\s*"([^"]+)")`, 'm'))
  if (!m) throw new Error(`mise.toml has no ${tool} version`)
  return m[1] ?? m[2]
}

if (args.includes('--snapshot')) {
  const body = await get(CATALOG_URL)
  JSON.parse(new TextDecoder().decode(body))
  mkdirSync(pluginsDir, { recursive: true })
  writeFileSync(catalogPath, body)
  console.log(`wrote ${catalogPath} (sha256 ${sha256Hex(body)})`)
  process.exit(0)
}

if (args.includes('--check')) {
  const live = JSON.parse(new TextDecoder().decode(await get(CATALOG_URL))) as Catalog
  process.stdout.write(driftReport(live, lockFromToml(readFileSync(lockPath, 'utf8'))))
  process.exit(0)
}

const catalogBytes = readFileSync(catalogPath)
const catalog = JSON.parse(catalogBytes.toString('utf8')) as Catalog
const exclude = loadExclude()
const index = (JSON.parse(readFileSync(indexPath, 'utf8')).manifests ?? []) as IndexEntry[]
const chosen = selectCandidates(catalog, exclude, DEFAULT_SIZE)
if (chosen.length < DEFAULT_SIZE) throw new Error(`only ${chosen.length} eligible manifests`)

async function download(repository: string, commit: string, path: string): Promise<Uint8Array> {
  const file = join(cacheDir, repository.replace('/', '__'), commit, path.replaceAll('/', '__'))
  if (existsSync(file)) return new Uint8Array(readFileSync(file))
  const body = await get(`https://raw.githubusercontent.com/${repository}/${commit}/${path}`)
  mkdirSync(dirname(file), { recursive: true })
  writeFileSync(file, body)
  return body
}

const plugins: LockedPlugin[] = []
for (const [i, c] of chosen.entries()) {
  const pin = await resolvePin(c, index, download, manifestPluginId)
  if (c.manifest.id && pin.plugin_id !== c.manifest.id) {
    console.error(`warning: ${c.repo.fullName} ${c.manifest.path}: manifest id ${pin.plugin_id} differs from catalog id ${c.manifest.id}`)
  }
  if (!pin.plugin_id) throw new Error(`${c.repo.fullName} ${c.manifest.path}: no plugin id`)
  plugins.push(lockedFrom(c, i + 1, pin))
}

const old: LockedPlugin[] = existsSync(lockPath) ? lockFromToml(readFileSync(lockPath, 'utf8')).plugins : []
const prevTools = existsSync(lockPath) ? lockFromToml(readFileSync(lockPath, 'utf8')).toolchains : {}
const lock: LockData = {
  catalog: 'catalog.json',
  catalog_sha256: sha256Hex(catalogBytes),
  catalog_generated_at: catalog.generatedAt,
  size: DEFAULT_SIZE,
  toolchains: {
    node: prevTools.node ?? NODE_VERSION,
    bun: miseVersion('bun'),
    go: prevTools.go ?? GO_VERSION,
    rust: miseVersion('rust'),
  },
  exclude,
  plugins,
}
writeFileSync(lockPath, renderLock(lock))
console.log(`wrote ${lockPath} with ${plugins.length} plugins`)
if (old.length) {
  const d = diffLocks(old, plugins)
  console.log(d.length ? `Changes against the previous lock (review these):\n${d.map(l => '  ' + l).join('\n')}` : 'No pin changes.')
}
