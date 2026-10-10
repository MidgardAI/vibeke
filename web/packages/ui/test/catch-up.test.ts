import { describe, expect, test } from 'bun:test';
import { AWAY_MS, buildCatchUp, excerpt, isAway, markSeen, snapshotRuns, sumChanges, type CatchUpBaseline } from '../src/lib/catch-up';
import { CatchUpStore, parseCatchUp } from '../src/lib/catch-up-store';
import type { KV } from '../src/lib/prefs';
import { buildTree } from '../src/lib/tree';
import { workspaceRows } from '../src/lib/workspaces';
import { dashboard, host, interaction, pane, run, tab, ws } from './fixtures';

const exec = (since_ms: number) => ({ value: 'idle' as const, since_ms, source: 'structured' as const, confidence: 1, detail: null });

type RunInit = NonNullable<Parameters<typeof run>[0]>;

function rowsOf(runs: RunInit[], extra: NonNullable<Parameters<typeof dashboard>[0]> = {}) {
  const d = dashboard({
    workspaces: [ws({ id: 'w1', name: 'api' }), ws({ id: 'w2', name: 'web' })],
    tabs: [tab({ id: 't1', workspace: 'w1' }), tab({ id: 't2', workspace: 'w2', layout: { Leaf: { pane: 'p2' } } })],
    panes: [pane({ id: 'p1', tab: 't1', workspace: 'w1' }), pane({ id: 'p2', tab: 't2', workspace: 'w2' })],
    runs: runs.map((r) => run(r)),
    ...extra,
  });
  return workspaceRows(buildTree([host('h1', d)], { pins: new Set(), seenDone: {} }));
}

const base: CatchUpBaseline = { at: 1000, runs: { 'h1/r1': { turns: 2, done_rev: 1 }, 'h1/r2': { turns: 0, done_rev: 0 } } };

describe('catch-up cards', () => {
  test('no baseline gives nothing', () => {
    expect(buildCatchUp(rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 5 }]), null)).toEqual([]);
  });

  test('turns finished since the baseline make a card; unchanged runs do not', () => {
    const rows = rowsOf([
      { id: 'r1', pane: 'p1', turns_completed: 5, done_rev: 3, execution: exec(5000), last_message: 'All **done**.\n\nTests pass.' },
      { id: 'r2', pane: 'p2', turns_completed: 0, done_rev: 0 },
    ]);
    const cards = buildCatchUp(rows, base);
    expect(cards).toHaveLength(1);
    expect(cards[0]).toMatchObject({ key: 'h1/w1', title: 'api', turns: 3, waiting: 0, pane: 'p1', lastMessage: 'All done. Tests pass.', since: 1000 });
    expect(cards[0]!.runs[0]).toMatchObject({ turns: 3, finished: true, isNew: false });
  });

  test('a run started after the baseline is new', () => {
    const rows = rowsOf([{ id: 'r9', pane: 'p2', turns_completed: 1, started_at_ms: 2000 }]);
    const cards = buildCatchUp(rows, base);
    expect(cards).toHaveLength(1);
    expect(cards[0]!.runs[0]).toMatchObject({ isNew: true, turns: 1 });
  });

  test('an old run missing from the baseline is not news', () => {
    expect(buildCatchUp(rowsOf([{ id: 'r9', pane: 'p2', turns_completed: 4, started_at_ms: 10 }]), base)).toEqual([]);
  });

  test('a request opened while away counts, an older one does not', () => {
    const rows = rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 2, done_rev: 1 }], { interactions: [interaction({ id: 'i1', run: 'r1', pane: 'p1', opened_at_ms: 3000 }), interaction({ id: 'i0', run: 'r2', pane: 'p2', opened_at_ms: 500 })] });
    const cards = buildCatchUp(rows, base);
    expect(cards.map((c) => [c.key, c.waiting])).toEqual([['h1/w1', 1]]);
  });

  test('most recent activity first', () => {
    const rows = rowsOf([
      { id: 'r1', pane: 'p1', turns_completed: 3, execution: exec(3000) },
      { id: 'r2', pane: 'p2', turns_completed: 1, execution: exec(9000) },
    ]);
    expect(buildCatchUp(rows, base).map((c) => c.key)).toEqual(['h1/w2', 'h1/w1']);
  });

  test('marking a card seen removes it, and only it', () => {
    const rows = rowsOf(
      [
        { id: 'r1', pane: 'p1', turns_completed: 3 },
        { id: 'r2', pane: 'p2', turns_completed: 1 },
      ],
      { interactions: [interaction({ id: 'i1', run: 'r1', pane: 'p1', opened_at_ms: 3000 })] },
    );
    const cards = buildCatchUp(rows, base);
    expect(cards).toHaveLength(2);
    const after = markSeen(base, rows, [cards.find((c) => c.key === 'h1/w1')!], 4000);
    expect(buildCatchUp(rows, after).map((c) => c.key)).toEqual(['h1/w2']);
    // New turns after the dismissal bring the card back.
    const later = rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 4 }, { id: 'r2', pane: 'p2', turns_completed: 1 }]);
    expect(buildCatchUp(later, after).map((c) => c.key)).toContain('h1/w1');
  });
});

describe('helpers', () => {
  test('excerpt flattens markdown and cuts at a word', () => {
    expect(excerpt(null)).toBeNull();
    expect(excerpt('  ')).toBeNull();
    expect(excerpt('```ts\nlet a = 1;\n```\nDone')).toBe('let a = 1; Done');
    const long = excerpt('word '.repeat(100), 30)!;
    expect(long.endsWith('…')).toBe(true);
    expect(long.length).toBeLessThanOrEqual(31);
  });

  test('sumChanges adds up lines and counts files', () => {
    expect(sumChanges([{ adds: 3, dels: 1 }, { adds: null, dels: undefined }, { adds: 2, dels: 0 }])).toEqual({ files: 3, adds: 5, dels: 1 });
    expect(sumChanges([])).toEqual({ files: 0, adds: 0, dels: 0 });
  });

  test('isAway needs a background time and the threshold', () => {
    expect(isAway(null, 1e9)).toBe(false);
    expect(isAway(1000, 1000 + AWAY_MS - 1)).toBe(false);
    expect(isAway(1000, 1000 + AWAY_MS)).toBe(true);
  });

  test('snapshotRuns keys runs by host and id', () => {
    const rows = rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 2, done_rev: 4 }]);
    expect(snapshotRuns(rows)).toEqual({ 'h1/r1': { turns: 2, done_rev: 4 } });
  });
});

function memoryKv(init: Record<string, string> = {}): KV & { data: Record<string, string> } {
  const data = { ...init };
  return { data, get: (k) => data[k] ?? null, set: (k, v) => void (data[k] = v), remove: (k) => void delete data[k] };
}

describe('catch-up store', () => {
  test('a long absence turns the catch-up on, a short one does not', () => {
    let now = 10_000;
    const kv = memoryKv();
    const s = new CatchUpStore(kv, () => now);
    s.load();
    const before = rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 1 }]);
    s.hidden(before, 0);
    now += 60_000;
    s.visible();
    expect(s.state.get().away).toBe(false);
    s.hidden(before, 0);
    now += AWAY_MS + 1;
    s.visible();
    expect(s.state.get().away).toBe(true);
    const after = rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 3 }]);
    expect(s.cards(after)).toHaveLength(1);
    s.dismiss(after, s.cards(after));
    expect(s.state.get().away).toBe(false);
    expect(s.cards(after)).toEqual([]);
  });

  test('pending cards keep their baseline when the app is hidden again', () => {
    let now = 10_000;
    const s = new CatchUpStore(memoryKv(), () => now);
    s.load();
    s.hidden(rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 1 }]), 0);
    now += AWAY_MS + 1;
    s.visible();
    const mid = rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 2 }]);
    s.hidden(mid, s.cards(mid).length);
    now += AWAY_MS + 1;
    s.visible();
    expect(s.cards(rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 2 }]))[0]!.turns).toBe(1);
  });

  test('state survives a restart and bad storage reads as empty', () => {
    let now = 10_000;
    const kv = memoryKv();
    const a = new CatchUpStore(kv, () => now);
    a.load();
    a.hidden(rowsOf([{ id: 'r1', pane: 'p1', turns_completed: 1 }]), 0);
    now += AWAY_MS * 2;
    const b = new CatchUpStore(kv, () => now);
    b.load();
    expect(b.state.get().away).toBe(true);
    expect(parseCatchUp('{nope')).toEqual({ hiddenAt: null, baseline: null, away: false });
    expect(parseCatchUp('{"hiddenAt":"x","baseline":{"at":1,"runs":{"a":{"turns":"2"}}}}').baseline?.runs).toEqual({});
  });
});
