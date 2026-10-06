import { describe, expect, test } from 'bun:test';
import { applyLatest, applyOlder, emptyTranscript, mergeTurns, ownMessages, turnMatches } from '../src/transcript';
import type { TranscriptPage, TranscriptTurn } from '../src/model';

const turn = (n: number, text = `t${n}`): TranscriptTurn => ({
  n,
  ts: null,
  items: [
    { kind: 'text', role: 'user', text: `ask ${text}` },
    { kind: 'thinking', text: 'hmm' },
    { kind: 'tool_call', tool: 'Bash', summary: `{"command":"ls ${n}"}`, id: `c${n}` },
    { kind: 'tool_result', summary: 'ok', id: `c${n}`, error: n === 3 },
    { kind: 'text', role: 'assistant', text: `done ${text}` },
  ],
});
const range = (a: number, b: number) => Array.from({ length: b - a + 1 }, (_, i) => turn(a + i));
/** What the server returns for `{limit, before}` over turns 1..total (gateway_api.rs `transcript`). */
function serve(total: number, limit: number, before?: number): TranscriptPage {
  const all = range(1, total).filter((t) => t.n < (before ?? Infinity));
  const page = all.slice(Math.max(0, all.length - limit));
  const first = page[0]?.n;
  return { run: 'r1', turns: page, next_before: first !== undefined && first > 1 ? first : null };
}

describe('transcript paging', () => {
  test('pages backwards with next_before until the start, no duplicates', () => {
    let s = applyLatest(emptyTranscript(), serve(70, 30));
    expect(s.turns.map((t) => t.n)).toEqual(range(41, 70).map((t) => t.n));
    expect(s.nextBefore).toBe(41);
    s = applyOlder(s, serve(70, 30, s.nextBefore!));
    expect(s.turns[0]!.n).toBe(11);
    expect(s.nextBefore).toBe(11);
    s = applyOlder(s, serve(70, 30, s.nextBefore!));
    expect(s.turns.map((t) => t.n)).toEqual(range(1, 70).map((t) => t.n));
    expect(s.nextBefore).toBeNull();
  });

  test('refreshing the newest page dedupes by n and keeps older pages and the cursor', () => {
    let s = applyLatest(emptyTranscript(), serve(70, 30));
    s = applyOlder(s, serve(70, 30, 41));
    // Two new turns arrive and the last one grew.
    const next = serve(72, 30);
    next.turns[next.turns.length - 1] = turn(72, 'grown');
    s = applyLatest(s, next);
    expect(s.turns.map((t) => t.n)).toEqual(range(11, 72).map((t) => t.n));
    expect(new Set(s.turns.map((t) => t.n)).size).toBe(s.turns.length);
    expect(s.turns.at(-1)!.items[0]!.text).toBe('ask grown');
    expect(s.nextBefore).toBe(11);
  });

  test('a gap, a new run or a shrunk transcript starts over', () => {
    const s = applyLatest(emptyTranscript(), serve(40, 10));
    const gap = applyLatest(s, serve(80, 10));
    expect(gap.turns[0]!.n).toBe(71);
    expect(gap.nextBefore).toBe(71);
    const other = applyLatest(s, { ...serve(40, 10), run: 'r2' });
    expect(other.run).toBe('r2');
    expect(applyLatest(s, serve(5, 10)).turns.length).toBe(5);
    expect(applyOlder(s, { ...serve(40, 10, 31), run: 'r2' })).toBe(s);
  });

  test('older page with a non-decreasing cursor stops paging; overlap is deduped', () => {
    let s = applyLatest(emptyTranscript(), serve(40, 10));
    s = applyOlder(s, { run: 'r1', turns: range(25, 35), next_before: 31 });
    expect(s.nextBefore).toBeNull();
    expect(s.turns.map((t) => t.n)).toEqual(range(25, 40).map((t) => t.n));
    expect(mergeTurns([turn(2), turn(1)], [turn(2, 'x'), { n: NaN } as TranscriptTurn]).map((t) => t.items[0]!.text)).toEqual(['ask t1', 'ask x']);
  });

  test('find and own messages', () => {
    const turns = range(1, 3);
    expect(turnMatches(turns[1]!, 'LS 2')).toBe(true);
    expect(turnMatches(turns[1]!, 'nope')).toBe(false);
    expect(turnMatches(turns[1]!, '  ')).toBe(true);
    expect(ownMessages(turns)).toEqual(['1:0', '2:0', '3:0']);
  });
});

describe('transcript timing fields', () => {
  const timed = (n: number, items: number, duration: number | null): TranscriptTurn => ({
    ...turn(n),
    items: turn(n).items.slice(0, items).map((it, i) => ({ ...it, ts: 1_000 * n + i * 250 })),
    duration_ms: duration,
    tool_count: 1,
    subagent_count: 0,
  });

  test('a live refresh replaces the growing turn with its newer timing, keeps older turns as they were', () => {
    let s = applyLatest(emptyTranscript(), { run: 'r1', turns: [timed(1, 5, 900), timed(2, 3, 400)], next_before: null });
    const first = s.turns[0];
    s = applyLatest(s, { run: 'r1', turns: [timed(2, 5, 1_000), timed(3, 1, 0)], next_before: 2 });
    expect(s.turns.map((t) => t.n)).toEqual([1, 2, 3]);
    expect(s.turns[0]).toBe(first);
    expect(s.turns[1]!.duration_ms).toBe(1_000);
    expect(s.turns[1]!.items).toHaveLength(5);
    expect(s.turns[1]!.items[4]!.ts).toBe(3_000);
    expect(s.turns[2]!.tool_count).toBe(1);
    // The newest page's cursor does not move the older-pages cursor.
    expect(s.nextBefore).toBeNull();
  });

  test('older pages keep their timing; missing fields stay absent (older servers)', () => {
    let s = applyLatest(emptyTranscript(), { run: 'r1', turns: [timed(5, 2, 50)], next_before: 5 });
    s = applyOlder(s, { run: 'r1', turns: [turn(4)], next_before: null });
    expect(s.turns.map((t) => [t.n, t.duration_ms ?? null])).toEqual([
      [4, null],
      [5, 50],
    ]);
  });
});
