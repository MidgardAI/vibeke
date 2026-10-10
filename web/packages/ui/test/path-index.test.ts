import { describe, expect, test } from 'bun:test';
import type { FsList } from '@vibeke/core';
import { MAX_DIRS_PER_ENSURE, PathIndex, firstExisting, parentDir } from '../src/lib/path-index';

const entry = (name: string, kind: 'file' | 'dir' = 'file', secret = false) => ({ name, kind, ignored: false, secret });

function fixture(dirs: Record<string, FsList['entries']>, truncated = new Set<string>()) {
  const calls: string[] = [];
  let now = 0;
  const idx = new PathIndex(
    async (dir) => {
      calls.push(dir);
      const entries = dirs[dir];
      if (!entries) throw new Error('missing');
      return { path: dir, entries, truncated: truncated.has(dir) };
    },
    () => now,
    1000,
  );
  return { idx, calls, tick: (ms: number) => (now += ms) };
}

describe('PathIndex', () => {
  test('one list per directory, then answers from the cache', async () => {
    const { idx, calls } = fixture({ '': [entry('a.ts'), entry('src', 'dir')], src: [entry('b.ts')] });
    expect(idx.has('src/b.ts')).toBeUndefined();
    const n = await idx.ensure(['a.ts', 'src/b.ts', 'src/c.ts', 'src/b.ts']);
    expect(n).toBe(2);
    expect(calls.sort()).toEqual(['', 'src']);
    expect(idx.has('a.ts')).toBe(true);
    expect(idx.has('src/b.ts')).toBe(true);
    expect(idx.has('src/c.ts')).toBe(false);
    expect(idx.has('src')).toBe(false);
    expect(await idx.ensure(['a.ts', 'src/b.ts'])).toBe(0);
    expect(calls.length).toBe(2);
  });

  test('directories, secrets and unlistable directories do not count as files', async () => {
    const { idx } = fixture({ '': [entry('src', 'dir'), entry('.env', 'file', true)] });
    await idx.ensure(['src', '.env', 'nope/x.ts']);
    expect(idx.has('src')).toBe(false);
    expect(idx.has('.env')).toBe(false);
    expect(idx.has('nope/x.ts')).toBe(false);
  });

  test('a truncated listing that lacks the name stays unknown', async () => {
    const { idx } = fixture({ '': [entry('a.ts')] }, new Set(['']));
    await idx.ensure(['a.ts', 'z.ts']);
    expect(idx.has('a.ts')).toBe(true);
    expect(idx.has('z.ts')).toBeUndefined();
  });

  test('old answers still count, and are refreshed by the next ensure', async () => {
    const { idx, calls, tick } = fixture({ '': [entry('a.ts')] });
    await idx.ensure(['a.ts']);
    tick(5000);
    expect(idx.has('a.ts')).toBe(true);
    expect(await idx.ensure(['a.ts'])).toBe(1);
    expect(calls.length).toBe(2);
  });

  test('concurrent ensures share one request', async () => {
    const { idx, calls } = fixture({ '': [entry('a.ts')] });
    await Promise.all([idx.ensure(['a.ts']), idx.ensure(['b.ts'])]);
    expect(calls.length).toBe(1);
  });

  test('a batch is capped; the rest comes with the next call', async () => {
    const dirs: Record<string, FsList['entries']> = {};
    const paths: string[] = [];
    for (let i = 0; i < MAX_DIRS_PER_ENSURE + 3; i++) {
      dirs[`d${i}`] = [entry('f.ts')];
      paths.push(`d${i}/f.ts`);
    }
    const { idx } = fixture(dirs);
    expect(await idx.ensure(paths)).toBe(MAX_DIRS_PER_ENSURE);
    expect(await idx.ensure(paths)).toBe(3);
    expect(idx.has(`d${MAX_DIRS_PER_ENSURE + 2}/f.ts`)).toBe(true);
  });

  test('firstExisting and parentDir', async () => {
    const { idx } = fixture({ '': [entry('x.ts')], web: [] });
    await idx.ensure(['x.ts', 'web/x.ts']);
    expect(firstExisting(idx, ['web/x.ts', 'x.ts'])).toBe('x.ts');
    expect(firstExisting(idx, ['web/x.ts'])).toBeNull();
    expect(parentDir('a/b/c.ts')).toBe('a/b');
    expect(parentDir('c.ts')).toBe('');
  });
});
