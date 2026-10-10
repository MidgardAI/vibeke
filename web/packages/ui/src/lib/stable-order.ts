// Stable list order. Rows must not jump under a finger: while a pointer is down on the list, or
// shortly after the last scroll or touch, the list keeps the order it showed. New data still
// reaches the rows in place (dot, badge, text); moves and new rows wait until the freeze ends.

/** How long after the last touch or scroll the order stays frozen. */
export const FREEZE_QUIET_MS = 1500;

export interface FreezeInput {
  pointerDown: boolean;
  /** Time of the last pointer, touch or scroll event; 0 = never. */
  lastActivityMs: number;
  nowMs: number;
  quietMs?: number;
}

export function isFrozen(i: FreezeInput): boolean {
  if (i.pointerDown) return true;
  return i.lastActivityMs > 0 && i.nowMs - i.lastActivityMs < (i.quietMs ?? FREEZE_QUIET_MS);
}

/** When to re-check a frozen list (ms from now), or null when it is not frozen. */
export function unfreezeDelay(i: FreezeInput): number | null {
  if (i.pointerDown) return null;
  if (!isFrozen(i)) return null;
  return Math.max(0, i.lastActivityMs + (i.quietMs ?? FREEZE_QUIET_MS) - i.nowMs) + 20;
}

export interface Section<R extends { key: string }> {
  id: string;
  rows: R[];
}

/**
 * Keep the sections and row order of `prev`, but show the fresh row objects from `next`. A row
 * that left the data disappears. A row that moved to another section stays where it was. A row
 * that is new, or whose section is new, waits for the freeze to end.
 */
export function freezeSections<R extends { key: string }>(prev: readonly Section<R>[] | null, next: readonly Section<R>[]): Section<R>[] {
  if (!prev) return next.map((s) => ({ id: s.id, rows: [...s.rows] }));
  const fresh = new Map<string, R>();
  for (const s of next) for (const r of s.rows) if (!fresh.has(r.key)) fresh.set(r.key, r);
  const out: Section<R>[] = [];
  for (const s of prev) {
    const rows = s.rows.map((r) => fresh.get(r.key)).filter((r): r is R => !!r);
    if (rows.length) out.push({ id: s.id, rows });
  }
  return out;
}
