// Scan mirrored plugin sources for direct use of the Herdr socket (spec 18 §8.5).
//   bun scripts/herdr-plugins-scan.ts --mirror <dir> [--only <plugin_id>]...
// Read-only: it runs `git grep` on the bare repositories at the pinned commits.
// Output is one JSON line per locked plugin: {plugin_id, raw_socket, hits: [{path, line}]}.
// A plugin uses the raw socket when its source names the socket path (HERDR_SOCKET_PATH,
// herdr.sock), or when one file both opens a unix socket and names Herdr API methods. Using the
// `herdr` command line tool is not a hit. Markdown files are ignored.
import { readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { lockFromToml } from './herdr-plugins/lib'

const root = resolve(import.meta.dir, '..')
const lockPath = join(root, 'tests/compat/herdr/0.9.3/plugins/top20.lock.toml')

const usage = 'usage: bun scripts/herdr-plugins-scan.ts --mirror <dir> [--only <plugin_id>]...'
const argv = process.argv.slice(2)
let mirror = ''
const only: string[] = []
for (let i = 0; i < argv.length; i++) {
  const a = argv[i]
  if (a === '--help' || a === '-h') {
    console.log(usage)
    process.exit(0)
  } else if (a === '--mirror') mirror = argv[++i] ?? ''
  else if (a === '--only') only.push(argv[++i] ?? '')
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

const SOCKET_NAME = '(HERDR_SOCKET|herdr\\.sock)'
const UNIX_CONNECT =
  '(net\\.createConnection|net\\.connect|createConnection\\(|AF_UNIX|UnixStream|UnixDatagram|net\\.Dial\\(.unix|DialUnix|connect_unix|unix_connect|socat|nc -U|curl.*--unix-socket)'
const API_METHOD = '"(method|command)"[[:space:]]*:[[:space:]]*"(pane|workspace|tab|agent|events|server|session)\\.[a-z_.]+"'
const NO_DOCS = [':(exclude)*.md', ':(exclude)*.mdx']

interface Hit {
  path: string
  line: number
}

/** `git grep -n` output lines are `<commit>:<path>:<line>:<text>`. */
function grep(bare: string, commit: string, patterns: string[], allMatch: boolean): Hit[] {
  const a = ['-C', bare, 'grep', '-n', '-I', '-E', '--no-color']
  if (allMatch) a.push('--all-match')
  for (const p of patterns) a.push('-e', p)
  a.push(commit, '--', '.', ...NO_DOCS)
  const r = Bun.spawnSync(['git', ...a], { stdout: 'pipe', stderr: 'pipe' })
  if (r.exitCode === 1) return []
  if (r.exitCode !== 0) throw new Error(`git grep: ${r.stderr.toString().trim()}`)
  const hits: Hit[] = []
  for (const l of r.stdout.toString().split('\n')) {
    if (!l.startsWith(commit + ':')) continue
    const m = l.slice(commit.length + 1).match(/^(.*?):(\d+):/)
    if (m) hits.push({ path: m[1], line: Number(m[2]) })
  }
  return hits
}

const lock = lockFromToml(readFileSync(lockPath, 'utf8'))
let failed = 0
for (const p of lock.plugins) {
  if (only.length && !only.includes(p.plugin_id)) continue
  const bare = join(mirror, ...p.repository.split('/'))
  try {
    const seen = new Set<string>()
    const hits: Hit[] = []
    const add = (hs: Hit[]) => {
      for (const h of hs) {
        const k = `${h.path}:${h.line}`
        if (!seen.has(k)) {
          seen.add(k)
          hits.push(h)
        }
      }
    }
    add(grep(bare, p.commit, [SOCKET_NAME], false))
    // Files that connect to a unix socket and also name Herdr API methods; report the method lines.
    const both = grep(bare, p.commit, [UNIX_CONNECT, API_METHOD], true)
    const files = new Set(both.map(h => h.path))
    add(grep(bare, p.commit, [API_METHOD], false).filter(h => files.has(h.path)))
    hits.sort((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : a.line - b.line))
    console.log(JSON.stringify({ plugin_id: p.plugin_id, raw_socket: hits.length > 0, hits }))
  } catch (e) {
    failed++
    console.error(`FAIL ${p.plugin_id}: ${(e as Error).message}`)
  }
}
process.exit(failed ? 1 : 0)
