// Put a voice transcript into the draft at the caret (or over the selection), adding a space
// only where it separates words.

export interface Inserted {
  text: string;
  /** Caret position after the inserted words. */
  caret: number;
}

export function insertAtCaret(cur: string, start: number, end: number, insert: string): Inserted {
  const words = insert.trim();
  const s = Math.max(0, Math.min(start, cur.length));
  const e = Math.max(s, Math.min(end, cur.length));
  if (!words) return { text: cur, caret: s };
  const before = cur.slice(0, s);
  const after = cur.slice(e);
  const lead = before && !/\s$/.test(before) ? ' ' : '';
  const trail = after && !/^\s/.test(after) ? ' ' : '';
  const mid = `${lead}${words}${trail}`;
  return { text: before + mid + after, caret: before.length + lead.length + words.length };
}
