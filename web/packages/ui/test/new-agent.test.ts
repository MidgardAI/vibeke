import { describe, expect, test } from 'bun:test';
import type { HarnessInfo } from '@vibeke/core';
import {
  branchProblem,
  buildStartParams,
  folderLabel,
  generateBranchName,
  normalizeFolder,
  opIdFor,
  pushRecent,
  recentOnly,
  resolveAgain,
  toggleFavorite,
} from '../src/lib/new-agent';

describe('branch names', () => {
  test('generated names have the agent/<adjective>-<noun>-<hex> shape and are valid', () => {
    for (let i = 0; i < 50; i++) {
      const n = generateBranchName();
      expect(n).toMatch(/^agent\/[a-z]+-[a-z]+-[0-9a-f]{4}$/);
      expect(branchProblem(n)).toBeNull();
    }
  });

  test('uses the injected random source', () => {
    expect(generateBranchName(() => 0)).toBe('agent/brisk-otter-0000');
    expect(generateBranchName(() => 0.999999)).toMatch(/-ffff$/);
  });

  test('accepts ordinary names', () => {
    for (const n of ['main', 'feature/login', 'fix-1.2', 'a/b/c', 'v1.0']) expect(branchProblem(n)).toBeNull();
  });

  test('rejects names git refuses', () => {
    expect(branchProblem('')).toBe('empty');
    expect(branchProblem('a b')).toBe('chars');
    expect(branchProblem('a~b')).toBe('chars');
    expect(branchProblem('a:b')).toBe('chars');
    expect(branchProblem('a\\b')).toBe('chars');
    expect(branchProblem('a[b')).toBe('chars');
    expect(branchProblem('a..b')).toBe('dots');
    expect(branchProblem('a/.hidden')).toBe('dots');
    expect(branchProblem('a//b')).toBe('slashes');
    expect(branchProblem('-x')).toBe('edge');
    expect(branchProblem('/x')).toBe('edge');
    expect(branchProblem('x/')).toBe('edge');
    expect(branchProblem('x.')).toBe('edge');
    expect(branchProblem('x.lock')).toBe('lock');
    expect(branchProblem('a.lock/b')).toBe('lock');
    expect(branchProblem('@')).toBe('at');
    expect(branchProblem('a@{b')).toBe('at');
    expect(branchProblem('x'.repeat(201))).toBe('long');
  });
});

describe('folder shortcuts', () => {
  test('normalizes and labels paths', () => {
    expect(normalizeFolder(' ~/code/app/ ')).toBe('~/code/app');
    expect(normalizeFolder('/')).toBe('/');
    expect(folderLabel('~/code/app/')).toBe('app');
    expect(folderLabel('~')).toBe('~');
  });

  test('recent folders move to the front, without duplicates, capped at 8', () => {
    let l: string[] = [];
    for (let i = 0; i < 10; i++) l = pushRecent(l, `~/p${i}`);
    expect(l).toHaveLength(8);
    expect(l[0]).toBe('~/p9');
    expect(pushRecent(l, '~/p5/')[0]).toBe('~/p5');
    expect(pushRecent(l, '~/p5').filter((x) => x === '~/p5')).toHaveLength(1);
    expect(pushRecent(['a'], '  ')).toEqual(['a']);
  });

  test('favourites toggle and stop at the cap', () => {
    expect(toggleFavorite([], '~/a/')).toEqual(['~/a']);
    expect(toggleFavorite(['~/a'], '~/a')).toEqual([]);
    const full = Array.from({ length: 8 }, (_, i) => `~/f${i}`);
    expect(toggleFavorite(full, '~/new')).toEqual(full);
    expect(toggleFavorite(full, '~/f3')).toHaveLength(7);
  });

  test('recent list hides favourites', () => {
    expect(recentOnly({ favorites: ['a'], recent: ['b', 'a', 'c'] })).toEqual(['b', 'c']);
  });
});

describe('again', () => {
  const harnesses: HarnessInfo[] = [
    { id: 'claude', display: 'Claude', capabilities: [], version_detected: '1.0' },
    { id: 'codex', display: 'Codex', capabilities: [], version_detected: null },
  ];
  const last = { harness: 'claude', where: { kind: 'workspace' as const, id: 'w1' }, worktree: true };

  test('repeats a start that is still possible', () => {
    expect(resolveAgain(last, [{ id: 'w1' }], harnesses)).toBe(last);
  });

  test('drops it when the workspace is gone or the harness is not installed', () => {
    expect(resolveAgain(undefined, [{ id: 'w1' }], harnesses)).toBeNull();
    expect(resolveAgain(last, [{ id: 'w2' }], harnesses)).toBeNull();
    expect(resolveAgain({ ...last, harness: 'codex' }, [{ id: 'w1' }], harnesses)).toBeNull();
    expect(resolveAgain({ ...last, where: { kind: 'folder', cwd: '~/x' } }, [], harnesses)).not.toBeNull();
  });
});

describe('start parameters', () => {
  test('an existing workspace', () => {
    expect(buildStartParams({ harness: 'claude', prompt: ' hi ', where: { kind: 'workspace', id: 'w1' }, worktree: null })).toEqual({
      harness: 'claude',
      prompt: 'hi',
      workspace: 'w1',
    });
  });

  test('a new folder', () => {
    expect(buildStartParams({ harness: 'claude', prompt: '', where: { kind: 'folder', cwd: '~/code/app/' }, worktree: null })).toEqual({
      harness: 'claude',
      new_workspace: { cwd: '~/code/app' },
    });
  });

  test('a worktree with and without a base', () => {
    const w = { kind: 'workspace', id: 'w1' } as const;
    expect(buildStartParams({ harness: 'c', prompt: '', where: w, worktree: { branch: ' agent/x ', base: 'main' } }).worktree).toEqual({ branch: 'agent/x', base: 'main' });
    expect(buildStartParams({ harness: 'c', prompt: '', where: w, worktree: { branch: 'b' } }).worktree).toEqual({ branch: 'b' });
  });
});

describe('op ids', () => {
  test('a retry with the same parameters keeps the id, changed parameters get a new one', () => {
    let n = 0;
    const make = () => `id${++n}`;
    const a = opIdFor(null, { x: 1 }, make);
    const b = opIdFor(a, { x: 1 }, make);
    expect(b).toBe(a);
    const c = opIdFor(b, { x: 2 }, make);
    expect(c.id).toBe('id2');
  });
});
