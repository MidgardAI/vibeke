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

/** A list row and the items it shows (a batch shows several; a card shows itself). */
export interface MemberRow {
  key: string;
  members: readonly string[];
}

export interface FrozenRows<R extends MemberRow> {
  rows: R[];
  /** Rows that arrived while frozen and wait for the freeze to end. */
  held: number;
}

/**
 * Keep the order of `prev` while the user touches the list. A row that is still there stays in
 * its place. When a row became another row (a card merged into a batch, or a batch split into
 * cards), the new row takes the old place. A row that left disappears. Only rows whose items were
 * never on screen are new: they wait, and `held` counts them for an "N new" pill.
 */
export function freezeRows<R extends MemberRow>(prev: readonly MemberRow[] | null, next: readonly R[]): FrozenRows<R> {
  if (!prev) return { rows: [...next], held: 0 };
  const byKey = new Map<string, R>();
  const byMember = new Map<string, R>();
  for (const r of next) {
    byKey.set(r.key, r);
    for (const m of r.members) if (!byMember.has(m)) byMember.set(m, r);
  }
  const placed = new Set<string>();
  const rows: R[] = [];
  const place = (r: R | undefined) => {
    if (!r || placed.has(r.key)) return;
    placed.add(r.key);
    rows.push(r);
  };
  const seen = new Set<string>();
  for (const p of prev) {
    for (const m of p.members) seen.add(m);
    if (byKey.has(p.key)) place(byKey.get(p.key));
    else for (const m of p.members) place(byMember.get(m));
  }
  let held = 0;
  for (const r of next) {
    if (placed.has(r.key)) continue;
    // A row with an item that was on screen keeps showing (at the end, so nothing above moves).
    if (r.members.some((m) => seen.has(m))) place(r);
    else held++;
  }
  return { rows, held };
}

/** Ignore taps for this long after a row moved under the pointer. */
export const MOVED_TAP_GUARD_MS = 400;
