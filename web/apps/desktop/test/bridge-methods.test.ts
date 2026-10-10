// Every API method the shared UI calls must be on the desktop's allow-list, or be listed here as
// deliberately not available to desktop windows. A new UI feature that forgets the list would
// otherwise fail (or hide itself) only in the desktop app.
import { expect, test } from 'bun:test';
import { readdirSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { RENDERER_METHODS } from '../src/shared/contract';

const NOT_ON_DESKTOP: Record<string, string> = {
  'push.test': 'Web Push only; the desktop shows its own notifications and hides the button',
};

function sources(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap((e) => {
    const p = join(dir, e.name);
    if (e.isDirectory()) return sources(p);
    return /\.tsx?$/.test(e.name) ? [p] : [];
  });
}

test('the desktop bridge allows every method the UI requests', () => {
  const used = new Set<string>();
  for (const f of sources(join(import.meta.dir, '../../../packages/ui/src'))) {
    for (const m of readFileSync(f, 'utf8').matchAll(/\brequest\(\s*['"]([a-z_]+(?:\.[a-z_]+)+)['"]/g)) used.add(m[1]!);
  }
  expect(used.size).toBeGreaterThan(20);
  const allowed = new Set<string>(RENDERER_METHODS);
  const missing = [...used].filter((m) => !allowed.has(m) && !(m in NOT_ON_DESKTOP)).sort();
  expect(missing).toEqual([]);
  const stale = Object.keys(NOT_ON_DESKTOP).filter((m) => !used.has(m) || allowed.has(m));
  expect(stale).toEqual([]);
});
