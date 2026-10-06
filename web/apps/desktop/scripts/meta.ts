// Version and build hash shared by the renderer (vite) and main (Bun.build) bundles.

import { execSync } from 'node:child_process';
import { readFileSync } from 'node:fs';

export function appVersion(): string {
  return (JSON.parse(readFileSync(new URL('../package.json', import.meta.url), 'utf8')) as { version: string }).version;
}

export function buildHash(): string {
  if (process.env.VIBEKE_BUILD_HASH) return process.env.VIBEKE_BUILD_HASH;
  try {
    const h = execSync('git rev-parse --short=12 HEAD', { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
    const dirty = execSync('git status --porcelain -- .', { stdio: ['ignore', 'pipe', 'ignore'] }).toString().trim();
    return dirty ? `${h}-dirty` : h;
  } catch {
    return 'dev';
  }
}
