import { describe, expect, test } from 'bun:test';
import { extractPathRefs, findLinks, looksLikePath, normalizePath, parsePathRef, pathCandidates, relativeTo } from '../src/lib/linkify';

describe('urls', () => {
  test('http and https only, trailing punctuation dropped', () => {
    const text = 'see https://example.com/a?b=1. and (http://x.test/p), ftp://nope.test and javascript:alert(1)';
    const spans = findLinks(text);
    const urls = spans.filter((s) => s.kind === 'url').map((s) => (s.kind === 'url' ? s.url : ''));
    expect(urls).toEqual(['https://example.com/a?b=1', 'http://x.test/p']);
    const first = spans[0]!;
    expect(text.slice(first.start, first.end)).toBe('https://example.com/a?b=1');
  });

  test('balanced parentheses stay in the url', () => {
    const [s] = findLinks('https://en.example.org/wiki/Foo_(bar) now');
    expect(s).toMatchObject({ kind: 'url', url: 'https://en.example.org/wiki/Foo_(bar)' });
  });

  test('the path inside a url is not a second link', () => {
    const spans = findLinks('https://example.com/src/app.ts:12');
    expect(spans.length).toBe(1);
    expect(spans[0]!.kind).toBe('url');
  });
});

describe('paths', () => {
  test('path with line and column', () => {
    const text = 'error at src/app.ts:42:7 here';
    const [s] = findLinks(text);
    expect(s).toMatchObject({ kind: 'path', raw: 'src/app.ts', line: 42, col: 7 });
    expect(text.slice(s!.start, s!.end)).toBe('src/app.ts:42:7');
  });

  test('a sentence-ending dot or colon is not part of the path', () => {
    expect(findLinks('Edit src/lib/a.ts.')[0]).toMatchObject({ raw: 'src/lib/a.ts' });
    expect(findLinks('src/lib/a.ts: failed')[0]).toMatchObject({ raw: 'src/lib/a.ts' });
  });

  test('TypeScript style position and brackets', () => {
    const text = 'src/a.ts(10,3): error';
    const [s] = findLinks(text);
    expect(s).toMatchObject({ raw: 'src/a.ts', line: 10, col: 3 });
    expect(text.slice(s!.start, s!.end)).toBe('src/a.ts(10,3)');
    expect(findLinks('(see ./docs/x.md)')[0]).toMatchObject({ raw: './docs/x.md' });
  });

  test('plain words, versions and flags are not paths', () => {
    expect(findLinks('hello world 1.2.3 v1.0 -rf --flag a/ // ..')).toEqual([]);
    expect(looksLikePath('README')).toBe(false);
    expect(looksLikePath('README.md')).toBe(true);
    expect(looksLikePath('src/lib')).toBe(true);
    expect(looksLikePath('../x/y.rs')).toBe(true);
  });

  test('@scope tokens are skipped', () => {
    expect(findLinks('npm i @scope/pkg.js')).toEqual([]);
  });

  test('parsePathRef', () => {
    expect(parsePathRef('a/b.ts:3')).toEqual({ raw: 'a/b.ts', line: 3 });
    expect(parsePathRef('b.ts')).toEqual({ raw: 'b.ts' });
    expect(parsePathRef('word')).toBeNull();
  });

  test('extractPathRefs is distinct and capped', () => {
    expect(extractPathRefs('a/b.ts a/b.ts:4 c.rs')).toEqual(['a/b.ts', 'c.rs']);
    const many = Array.from({ length: 50 }, (_, i) => `d/f${i}.ts`).join(' ');
    expect(extractPathRefs(many, 10).length).toBe(10);
  });
});

describe('workspace paths', () => {
  test('normalizePath', () => {
    expect(normalizePath('./a/./b/../c.ts')).toBe('a/c.ts');
    expect(normalizePath('../x')).toBeNull();
    expect(normalizePath('')).toBeNull();
  });

  test('relativeTo', () => {
    expect(relativeTo('/repo', '/repo/web/a.ts')).toBe('web/a.ts');
    expect(relativeTo('/repo', '/repo')).toBe('');
    expect(relativeTo('/repo', '/repository/a')).toBeNull();
  });

  test('candidates: relative to the pane directory first, then the root', () => {
    const o = { repoRoot: '/repo', cwd: '/repo/web' };
    expect(pathCandidates('src/a.ts', o)).toEqual(['web/src/a.ts', 'src/a.ts']);
    expect(pathCandidates('../docs/x.md', o)).toEqual(['docs/x.md']);
    expect(pathCandidates('src/a.ts', { repoRoot: '/repo', cwd: '/repo' })).toEqual(['src/a.ts']);
  });

  test('absolute paths only inside the repository; home and escapes are rejected', () => {
    const o = { repoRoot: '/repo', cwd: '/repo' };
    expect(pathCandidates('/repo/src/a.ts', o)).toEqual(['src/a.ts']);
    expect(pathCandidates('/etc/passwd', o)).toEqual([]);
    expect(pathCandidates('~/a.ts', o)).toEqual([]);
    expect(pathCandidates('../../x.ts', o)).toEqual([]);
    expect(pathCandidates('/repo/src/a.ts', { cwd: '/repo' })).toEqual([]);
  });
});
