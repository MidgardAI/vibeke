// Remembers what the user had seen when the app went to the background, and whether they were
// away long enough for a catch-up. Persists in the shell's small key-value storage. The cards
// themselves are derived (lib/catch-up.ts) so they stay current while shown.

import { AWAY_MS, buildCatchUp, isAway, markSeen, snapshotRuns, type CatchUpBaseline, type CatchUpCard } from './catch-up';
import type { KV } from './prefs';
import { ValueStore } from './store';
import type { WorkspaceRow } from './workspaces';

const KEY = 'vibeke.catchup';

export interface CatchUpState {
  baseline: CatchUpBaseline | null;
  /** The user was away long enough; cards show until they are dismissed. */
  away: boolean;
}

interface Persisted {
  hiddenAt: number | null;
  baseline: CatchUpBaseline | null;
  /** Cards were showing (and may still be unread) when the state was saved. */
  away: boolean;
}

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);

/** Parse stored JSON leniently; anything unexpected reads as "nothing saved". */
export function parseCatchUp(raw: string | null): Persisted {
  const none: Persisted = { hiddenAt: null, baseline: null, away: false };
  let v: unknown;
  try {
    v = raw ? JSON.parse(raw) : null;
  } catch {
    return none;
  }
  if (!isObj(v)) return none;
  const hiddenAt = typeof v.hiddenAt === 'number' && Number.isFinite(v.hiddenAt) ? v.hiddenAt : null;
  const b = v.baseline;
  if (!isObj(b) || typeof b.at !== 'number' || !isObj(b.runs)) return { hiddenAt, baseline: null, away: false };
  const runs: CatchUpBaseline['runs'] = {};
  for (const [k, m] of Object.entries(b.runs)) {
    if (isObj(m) && typeof m.turns === 'number' && typeof m.done_rev === 'number') runs[k] = { turns: m.turns, done_rev: m.done_rev };
  }
  const seen: Record<string, number> = {};
  if (isObj(b.seen)) for (const [k, n] of Object.entries(b.seen)) if (typeof n === 'number') seen[k] = n;
  return { hiddenAt, baseline: { at: b.at, runs, seen }, away: v.away === true };
}

export class CatchUpStore {
  readonly state = new ValueStore<CatchUpState>({ baseline: null, away: false });
  private hiddenAt: number | null = null;

  constructor(
    private readonly kv: KV,
    private readonly now: () => number,
    private readonly awayMs = AWAY_MS,
  ) {}

  /** Read the saved state (app start): a long gap since the last background means "away". */
  load(): void {
    let raw: string | null = null;
    try {
      raw = this.kv.get(KEY);
    } catch {
      // storage unavailable: no catch-up
    }
    const p = parseCatchUp(raw);
    this.hiddenAt = p.hiddenAt;
    this.state.set({ baseline: p.baseline, away: p.baseline !== null && (p.away || isAway(p.hiddenAt, this.now(), this.awayMs)) });
  }

  /** The window went to the background. `pending` = cards still waiting: keep their baseline. */
  hidden(rows: readonly WorkspaceRow[], pending: number): void {
    const at = this.now();
    this.hiddenAt = at;
    const cur = this.state.get();
    const keep = cur.away && pending > 0 && cur.baseline !== null;
    const baseline = keep ? cur.baseline : { at, runs: snapshotRuns(rows) };
    this.state.set({ baseline, away: keep });
    this.save();
  }

  /** The window came back. */
  visible(): void {
    const cur = this.state.get();
    const away = cur.away || (cur.baseline !== null && isAway(this.hiddenAt, this.now(), this.awayMs));
    this.hiddenAt = null;
    this.state.set({ ...cur, away });
    this.save();
  }

  cards(rows: readonly WorkspaceRow[]): CatchUpCard[] {
    const s = this.state.get();
    return s.away ? buildCatchUp(rows, s.baseline) : [];
  }

  /** Mark these cards seen (a dismissed card, or all of them). */
  dismiss(rows: readonly WorkspaceRow[], cards: readonly CatchUpCard[]): void {
    const s = this.state.get();
    if (!s.baseline || !cards.length) return;
    const baseline = markSeen(s.baseline, rows, cards, this.now());
    const away = buildCatchUp(rows, baseline).length > 0;
    this.state.set({ baseline, away });
    this.save();
  }

  private save(): void {
    try {
      const s = this.state.get();
      this.kv.set(KEY, JSON.stringify({ hiddenAt: this.hiddenAt, baseline: s.baseline, away: s.away } satisfies Persisted));
    } catch {
      // storage full or unavailable: the next start shows no catch-up
    }
  }
}
