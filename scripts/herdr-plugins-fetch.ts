// Mirror the locked Herdr plugin repositories at their pinned commits (spec 18 §6, §7.1).
//   bun scripts/herdr-plugins-fetch.ts --mirror <dir> [--only <plugin_id>]... [--source-base <url>]
// Each repository becomes a bare git repository at <dir>/<owner>/<repo>, so that
// VIBEKE_PLUGIN_GIT_BASE=file://<dir> works with `vibeke plugin install owner/repo@<commit>`.
// The script only runs `git init` and `git fetch`. It never runs anything from the repositories.
import { mkdirSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { sha256Hex, lockFromToml, type LockedPlugin } from './herdr-plugins/lib'

const root = resolve(import.meta.dir, '..')
const lockPath = join(root, 'tests/compat/herdr/0.9.3/plugins/top20.lock.toml')

const usage = 'usage: bun scripts/herdr-plugins-fetch.ts --mirror <dir> [--only <plugin_id>]... [--source-base <url>]'
const argv = process.argv.slice(2)
let mirror = ''
let sourceBase = 'https://github.com/'
const only: string[] = []
for (let i = 0; i < argv.length; i++) {
  const a = argv[i]
  if (a === '--help' || a === '-h') {
    console.log(usage)
    process.exit(0)
  } else if (a === '--mirror') mirror = argv[++i] ?? ''
  else if (a === '--only') only.push(argv[++i] ?? '')
  else if (a === '--source-base') sourceBase = argv[++i] ?? sourceBase
  else {
    console.error(`unknown argument ${a}\n${usage}`)
    process.exit(2)
  }
}
if (!mirror) {
  console.error(usage)
  process.exit(2)
}
mirror = resolve(mirror)
if (!sourceBase.endsWith('/')) sourceBase += '/'

const env = { ...process.env, GIT_TERMINAL_PROMPT: '0', GIT_CONFIG_NOSYSTEM: '1' }

function git(cwd: string, ...a: string[]): string {
  const p = Bun.spawnSync(['git', '-c', 'core.hooksPath=/dev/null', ...a], { cwd, env, stdout: 'pipe', stderr: 'pipe' })
  if (p.exitCode !== 0) throw new Error(`git ${a.join(' ')}: ${p.stderr.toString().trim()}`)
  return p.stdout.toString()
}
function gitBytes(cwd: string, ...a: string[]): Uint8Array {
  const p = Bun.spawnSync(['git', ...a], { cwd, env, stdout: 'pipe', stderr: 'pipe' })
  if (p.exitCode !== 0) throw new Error(`git ${a.join(' ')}: ${p.stderr.toString().trim()}`)
  return new Uint8Array(p.stdout)
}

const lock = lockFromToml(readFileSync(lockPath, 'utf8'))
const wanted = lock.plugins.filter(p => only.length === 0 || only.includes(p.plugin_id))
for (const id of only) if (!lock.plugins.some(p => p.plugin_id === id)) throw new Error(`no locked plugin ${id}`)

const byRepo = new Map<string, LockedPlugin[]>()
for (const p of wanted) byRepo.set(p.repository, [...(byRepo.get(p.repository) ?? []), p])

let failed = 0
for (const [repository, plugins] of byRepo) {
  const bare = join(mirror, ...repository.split('/'))
  try {
    mkdirSync(bare, { recursive: true })
    git(bare, 'init', '-q', '--bare', '-b', 'main')
    git(bare, 'config', 'uploadpack.allowAnySHA1InWant', 'true')
    const commits = [...new Set(plugins.map(p => p.commit))]
    for (const commit of commits) {
      git(bare, 'fetch', '-q', '--depth', '1', '--no-tags', `${sourceBase}${repository}.git`, commit)
      const got = git(bare, 'rev-parse', '--verify', `${commit}^{commit}`).trim()
      if (got !== commit) throw new Error(`fetched ${got}, expected ${commit}`)
      git(bare, 'update-ref', `refs/pinned/${commit}`, commit)
    }
    git(bare, 'update-ref', 'refs/heads/main', commits[0])
    git(bare, 'symbolic-ref', 'HEAD', 'refs/heads/main')
    for (const p of plugins) {
      const sum = sha256Hex(gitBytes(bare, 'show', `${p.commit}:${p.manifest_path}`))
      if (sum !== p.manifest_sha256) throw new Error(`${p.plugin_id}: manifest sha256 ${sum} differs from the lock`)
      console.log(`ok ${p.plugin_id} ${repository}@${p.commit.slice(0, 12)}`)
    }
  } catch (e) {
    failed++
    console.error(`FAIL ${repository}: ${(e as Error).message}`)
  }
}
console.log(`mirror ready at ${mirror}: use VIBEKE_PLUGIN_GIT_BASE=file://${mirror}`)
process.exit(failed ? 1 : 0)
