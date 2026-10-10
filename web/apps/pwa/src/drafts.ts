// Unsent composer text that survives reloads: one localStorage entry per host and pane. Entries
// expire after 48 hours and are swept on start; the number and size of entries are bounded so a
// long-lived install cannot fill the storage.

export const DRAFT_MAX_AGE_MS = 48 * 60 * 60 * 1000;
export const DRAFT_MAX_ENTRIES = 50;
export const DRAFT_MAX_CHARS = 20_000;
export const DRAFT_PREFIX = 'vibeke.draft.';

export interface DraftStorage {
  readonly length: number;
  key(i: number): string | null;
  getItem(k: string): string | null;
  setItem(k: string, v: string): void;
  removeItem(k: string): void;
}

interface Entry {
  t: string;
  at: number;
}

const keyOf = (host: string, pane: string): string => `${DRAFT_PREFIX}${host}/${pane}`;

function parse(raw: string | null): Entry | null {
  if (!raw) return null;
  try {
    const v = JSON.parse(raw) as Partial<Entry>;
    return typeof v.t === 'string' && typeof v.at === 'number' ? { t: v.t, at: v.at } : null;
  } catch {
    return null;
  }
}

export function draftKeys(s: DraftStorage): string[] {
  const out: string[] = [];
  for (let i = 0; i < s.length; i++) {
    const k = s.key(i);
    if (k?.startsWith(DRAFT_PREFIX)) out.push(k);
  }
  return out;
}

/** Remove expired and unreadable entries, then the oldest ones beyond the bound. */
export function sweepDrafts(s: DraftStorage, now: number): void {
  const live: { k: string; at: number }[] = [];
  for (const k of draftKeys(s)) {
    const e = parse(s.getItem(k));
    if (!e || e.t === '' || now - e.at >= DRAFT_MAX_AGE_MS) s.removeItem(k);
    else live.push({ k, at: e.at });
  }
  live.sort((a, b) => a.at - b.at);
  for (const x of live.slice(0, Math.max(0, live.length - DRAFT_MAX_ENTRIES))) s.removeItem(x.k);
}

export function createDrafts(storage: () => DraftStorage | null, now: () => number) {
  const store = (): DraftStorage | null => {
    try {
      return storage();
    } catch {
      return null;
    }
  };
  return {
    async get(host: string, pane: string): Promise<string> {
      const s = store();
      if (!s) return '';
      const e = parse(s.getItem(keyOf(host, pane)));
      if (!e) return '';
      if (now() - e.at >= DRAFT_MAX_AGE_MS) {
        s.removeItem(keyOf(host, pane));
        return '';
      }
      return e.t;
    },
    async set(host: string, pane: string, text: string): Promise<void> {
      const s = store();
      if (!s) throw new Error('storage unavailable');
      const k = keyOf(host, pane);
      if (text === '') return s.removeItem(k);
      const entry: Entry = { t: text.slice(0, DRAFT_MAX_CHARS), at: now() };
      const isNew = s.getItem(k) === null;
      try {
        s.setItem(k, JSON.stringify(entry));
      } catch {
        // Quota: free space by dropping expired and the oldest drafts, then try once more.
        sweepDrafts(s, now());
        s.setItem(k, JSON.stringify(entry));
      }
      if (isNew && draftKeys(s).length > DRAFT_MAX_ENTRIES) sweepDrafts(s, now());
    },
    sweep(): void {
      const s = store();
      if (s) sweepDrafts(s, now());
    },
  };
}
