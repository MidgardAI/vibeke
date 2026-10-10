import { describe, expect, test } from 'bun:test';
import { DRAFT_MAX_AGE_MS, DRAFT_MAX_CHARS, DRAFT_MAX_ENTRIES, createDrafts, draftKeys, type DraftStorage } from '../src/drafts';

function memory(quota = Infinity): DraftStorage & { map: Map<string, string> } {
  const map = new Map<string, string>();
  return {
    map,
    get length() {
      return map.size;
    },
    key: (i) => [...map.keys()][i] ?? null,
    getItem: (k) => map.get(k) ?? null,
    setItem(k, v) {
      if (map.size >= quota && !map.has(k)) throw new Error('quota');
      map.set(k, v);
    },
    removeItem: (k) => void map.delete(k),
  };
}

describe('drafts', () => {
  test('save and read per host and pane; empty text removes the entry', async () => {
    const s = memory();
    const d = createDrafts(() => s, () => 1000);
    await d.set('h1', 'p1', 'hello');
    await d.set('h1', 'p2', 'other');
    expect(await d.get('h1', 'p1')).toBe('hello');
    expect(await d.get('h2', 'p1')).toBe('');
    await d.set('h1', 'p1', '');
    expect(await d.get('h1', 'p1')).toBe('');
    expect(draftKeys(s).length).toBe(1);
  });
  test('drafts older than 48 hours are gone', async () => {
    const s = memory();
    let now = 0;
    const d = createDrafts(() => s, () => now);
    await d.set('h', 'p', 'old');
    now = DRAFT_MAX_AGE_MS - 1;
    expect(await d.get('h', 'p')).toBe('old');
    now = DRAFT_MAX_AGE_MS;
    expect(await d.get('h', 'p')).toBe('');
    expect(s.map.size).toBe(0);
  });
  test('the sweep drops expired, unreadable and surplus entries but keeps other keys', async () => {
    const s = memory();
    let now = 0;
    const d = createDrafts(() => s, () => now);
    s.setItem('vibeke.prefs', '{}');
    s.setItem('vibeke.draft.bad', 'not json');
    await d.set('h', 'stale', 'x');
    now = DRAFT_MAX_AGE_MS + 10;
    for (let i = 0; i < DRAFT_MAX_ENTRIES + 5; i++) {
      now += 1;
      await d.set('h', `p${i}`, 'fresh');
    }
    d.sweep();
    expect(s.getItem('vibeke.prefs')).toBe('{}');
    expect(s.getItem('vibeke.draft.bad')).toBeNull();
    expect(s.getItem('vibeke.draft.h/stale')).toBeNull();
    expect(draftKeys(s).length).toBe(DRAFT_MAX_ENTRIES);
    expect(s.getItem('vibeke.draft.h/p0')).toBeNull();
  });
  test('long text is cut, and a full storage makes room', async () => {
    const s = memory(2);
    let now = 0;
    const d = createDrafts(() => s, () => now);
    await d.set('h', 'a', 'x'.repeat(DRAFT_MAX_CHARS + 50));
    expect((await d.get('h', 'a')).length).toBe(DRAFT_MAX_CHARS);
    await d.set('h', 'b', 'y');
    now = DRAFT_MAX_AGE_MS + 1;
    await d.set('h', 'c', 'z');
    expect(await d.get('h', 'c')).toBe('z');
  });
  test('missing storage rejects a save so the UI can warn', async () => {
    const d = createDrafts(() => null, () => 0);
    expect(await d.get('h', 'p')).toBe('');
    await expect(d.set('h', 'p', 'x')).rejects.toThrow();
  });
});
