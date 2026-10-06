// Transcript paging (`agent.transcript`, spec 16 §9.1). The server numbers turns from the start
// of the transcript (`n`, stable across calls) and pages backwards with `before`. Pages are
// merged by `n`, so a refresh of the newest page and older pages never duplicate a turn, and the
// newest turn (still growing while the agent works) is replaced by its latest version.

import type { TranscriptPage, TranscriptTurn } from './model';

export interface TranscriptState {
  run: string | null;
  /** Ascending by `n`, unique. */
  turns: TranscriptTurn[];
  /** `before` for the next older page; null = the start is loaded. */
  nextBefore: number | null;
}

export const emptyTranscript = (): TranscriptState => ({ run: null, turns: [], nextBefore: null });

const validTurn = (t: unknown): t is TranscriptTurn =>
  !!t && typeof t === 'object' && typeof (t as TranscriptTurn).n === 'number' && Number.isFinite((t as TranscriptTurn).n);

/** Union by `n`, ascending. Turns in `b` replace turns in `a` with the same `n`. */
export function mergeTurns(a: readonly TranscriptTurn[], b: readonly TranscriptTurn[]): TranscriptTurn[] {
  const m = new Map<number, TranscriptTurn>();
  for (const t of a) if (validTurn(t)) m.set(t.n, t);
  for (const t of b) if (validTurn(t)) m.set(t.n, { ...t, items: Array.isArray(t.items) ? t.items : [] });
  return [...m.values()].sort((x, y) => x.n - y.n);
}

const cursor = (v: unknown): number | null => (typeof v === 'number' && Number.isFinite(v) && v > 1 ? v : null);

/**
 * Apply the newest page (no `before`). Keeps already-loaded older turns when the page connects
 * to them; starts over when the run changed, the transcript shrank, or a gap opened (more new
 * turns than one page).
 */
export function applyLatest(s: TranscriptState, page: TranscriptPage): TranscriptState {
  const turns = (page.turns ?? []).filter(validTurn);
  const fresh: TranscriptState = { run: page.run ?? s.run, turns: mergeTurns([], turns), nextBefore: cursor(page.next_before) };
  if (s.run !== null && page.run !== undefined && page.run !== s.run) return fresh;
  if (!s.turns.length || !turns.length) return fresh;
  const lastHave = s.turns[s.turns.length - 1]!.n;
  const firstNew = Math.min(...turns.map((t) => t.n));
  const lastNew = Math.max(...turns.map((t) => t.n));
  if (firstNew > lastHave + 1 || lastNew < lastHave) return fresh;
  return { run: fresh.run, turns: mergeTurns(s.turns, turns), nextBefore: s.nextBefore };
}

/** Apply an older page (requested with `before = s.nextBefore`). */
export function applyOlder(s: TranscriptState, page: TranscriptPage): TranscriptState {
  if (s.run !== null && page.run !== undefined && page.run !== s.run) return s;
  const turns = (page.turns ?? []).filter(validTurn);
  const next = cursor(page.next_before);
  // Never move the cursor forward (a confused server must not loop the button).
  const nextBefore = next !== null && s.nextBefore !== null && next >= s.nextBefore ? null : next;
  return { run: s.run ?? page.run ?? null, turns: mergeTurns(turns, s.turns), nextBefore };
}

/** Searchable text of an item (what the History screen shows). */
export const itemText = (it: TranscriptTurn['items'][number]): string =>
  [it.tool, it.summary, it.text].filter((x): x is string => typeof x === 'string' && x.length > 0).join('\n');

/** Case-insensitive match anywhere in the turn. */
export function turnMatches(turn: TranscriptTurn, q: string): boolean {
  const needle = q.trim().toLowerCase();
  if (!needle) return true;
  return turn.items.some((it) => itemText(it).toLowerCase().includes(needle));
}

/** DOM-stable keys (`n:index`) of the user's own messages, in order. */
export function ownMessages(turns: readonly TranscriptTurn[]): string[] {
  const out: string[] = [];
  for (const t of turns) t.items.forEach((it, i) => it.kind === 'text' && it.role === 'user' && out.push(`${t.n}:${i}`));
  return out;
}
