import { describe, expect, test } from 'bun:test';
import { RpcError, type DeskHit, type SearchHit } from '@vibeke/core';
import { PER_TARGET, classifySearchError, matchScore, mergeResults, searchableQuery, toMs, type HostHits, type SearchLookup } from '../src/lib/search-all';

const DAY = 86_400_000;
const NOW = 1_700_000_000_000;

const lookup: SearchLookup = {
  pane: (h, p) => (h === 'h1' && p === 'p1' ? { workspace: 'w1', workspaceName: 'api', title: 'claude', harness: 'claude', pane: 'p1' } : null),
  run: (h, r) => (h === 'h1' && r === 'r1' ? { workspace: 'w1', workspaceName: 'api', title: 'claude', harness: 'claude', pane: 'p1' } : null),
};

const sb = (p: Partial<SearchHit> = {}): SearchHit => ({ source: 'live', text: 'error: build failed', pane: 'p1', line: 10, ts: NOW / 1000, ...p });
const dk = (p: Partial<DeskHit> = {}): DeskHit => ({ session: 's1', harness: 'codex', turn: 1, role: 'assistant', kind: 'message', ts: NOW, snippet: 'the build failed twice', ...p });
const hh = (p: Partial<HostHits> = {}): HostHits => ({ host: 'h1', hostName: 'Laptop', scrollback: [], desk: [], ...p });

describe('search merge and rank', () => {
  test('timestamps in seconds or milliseconds both become milliseconds', () => {
    expect(toMs(1_700_000_000)).toBe(1_700_000_000_000);
    expect(toMs(1_700_000_000_000)).toBe(1_700_000_000_000);
    expect(toMs(undefined)).toBe(0);
    expect(toMs(-5)).toBe(0);
  });

  test('a phrase match outscores a partial one', () => {
    expect(matchScore('build failed', 'error: build failed')).toBeGreaterThan(matchScore('build failed', 'failed to build'));
    expect(matchScore('', 'x')).toBe(0);
  });

  test('results carry host, workspace and harness, and open the live pane', () => {
    const [r] = mergeResults('build failed', [hh({ scrollback: [sb()] })], lookup, NOW);
    expect(r).toMatchObject({ kind: 'scrollback', host: 'h1', hostName: 'Laptop', workspace: 'w1', workspaceName: 'api', harness: 'claude', pane: 'p1', line: 10, live: true });
  });

  test('session hits link to a live run and keep past ones without a pane', () => {
    const out = mergeResults('build', [hh({ desk: [dk({ live: { run: 'r1', pane: 'p1' } }), dk({ session: 's2', turn: 3 })] })], lookup, NOW);
    expect(out).toHaveLength(2);
    const live = out.find((r) => r.id.includes('/s1/'))!;
    const past = out.find((r) => r.id.includes('/s2/'))!;
    expect(live).toMatchObject({ pane: 'p1', run: 'r1', live: true, workspace: 'w1' });
    expect(past).toMatchObject({ pane: null, run: null, live: false });
    expect(out[0]).toBe(live);
  });

  test('hosts merge into one list and newer hits rank higher', () => {
    const out = mergeResults(
      'build',
      [hh({ desk: [dk({ session: 'old', ts: NOW - 60 * DAY })] }), hh({ host: 'h2', hostName: 'Server', desk: [dk({ session: 'new', ts: NOW - DAY })] })],
      lookup,
      NOW,
    );
    expect(out.map((r) => r.host)).toEqual(['h2', 'h1']);
  });

  test('duplicates collapse and one pane cannot fill the list', () => {
    const many = Array.from({ length: 10 }, (_, i) => sb({ line: i + 1, text: `build failed ${i}` }));
    const out = mergeResults('build', [hh({ scrollback: [...many, sb({ line: 1, text: 'build failed 0' })] })], lookup, NOW);
    expect(out).toHaveLength(PER_TARGET);
  });

  test('the limit applies', () => {
    const hits = Array.from({ length: 30 }, (_, i) => dk({ session: `s${i}` }));
    expect(mergeResults('build', [hh({ desk: hits })], lookup, NOW, 5)).toHaveLength(5);
  });

  test('errors: refusals and old gateways are quiet', () => {
    const e = (kind: string, code = -32000) => new RpcError('search.query', { code, message: kind, data: { kind } });
    expect(classifySearchError(e('forbidden'))).toBe('refused');
    expect(classifySearchError(e('method_not_found', -32601))).toBe('unsupported');
    expect(classifySearchError(new Error('timeout'))).toBe('error');
  });

  test('queries need two characters', () => {
    expect(searchableQuery(' a ')).toBeNull();
    expect(searchableQuery(' ab ')).toBe('ab');
  });
});
