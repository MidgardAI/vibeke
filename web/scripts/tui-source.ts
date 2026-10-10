// Fingerprint the Rust workspace inputs so a web build cannot package a stale WASM module.
import { createHash } from 'node:crypto';
import { execFileSync } from 'node:child_process';
import { readFileSync, existsSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
const root = resolve(fileURLToPath(new URL('../../', import.meta.url)));
export function tuiSourceDigest(): string {
  const files = execFileSync('git', ['ls-files', '-z', '--cached', '--others', '--exclude-standard', '--', 'Cargo.toml', 'Cargo.lock', 'crates', 'scripts/build-wasm-tui.sh'], { cwd: root }).toString().split('\0').filter(Boolean).sort();
  const hash = createHash('sha256');
  for (const file of [...new Set(files)]) {
    const path = resolve(root, file);
    const bytes = existsSync(path) ? readFileSync(path) : Buffer.from('deleted');
    hash.update(`${file}\0${bytes.length}\0`).update(bytes);
  }
  return hash.digest('hex');
}
if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) console.log(tuiSourceDigest());
