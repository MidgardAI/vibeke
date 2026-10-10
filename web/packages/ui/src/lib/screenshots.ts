// Screenshots of a workspace (`screenshot.list`): the pure parts of the Screenshots tab. Ordering,
// the unread count behind the tab badge, caption fallbacks and a byte-bounded cache for decoded
// images. No React and no DOM, so the unit tests import it directly.

import type { AppEvent, ScreenshotMeta } from '@vibeke/core';

/** `screenshot.get {inline: true}` returns bytes up to this size; bigger images are not requested. */
export const INLINE_MAX_BYTES = 8 * 1024 * 1024;
/** How many screenshots one workspace list keeps. */
export const LIST_LIMIT = 100;

export const isAgentShot = (s: Pick<ScreenshotMeta, 'environment'>): boolean => s.environment?.kind === 'agent';

/** Too big to fetch inline: the card shows a badge and no thumbnail. */
export const tooBigToInline = (s: Pick<ScreenshotMeta, 'bytes'>): boolean => s.bytes > INLINE_MAX_BYTES;

/**
 * What a card or the viewer calls the image: the agent's caption, else the attached file's name,
 * else the page title, else the environment label (browser screenshots), else the handle.
 */
export function captionOf(s: Pick<ScreenshotMeta, 'caption' | 'source_name' | 'title' | 'label' | 'handle'>): string {
  for (const v of [s.caption, s.source_name, s.title, s.label]) {
    const x = v?.trim();
    if (x) return x;
  }
  return s.handle;
}

/** A file name for Save: the source file's name, else `<handle>.<ext>`. */
export function saveName(s: Pick<ScreenshotMeta, 'source_name' | 'handle' | 'mime'>): string {
  const src = s.source_name?.trim();
  if (src) return src.replace(/[\\/]/g, '_');
  return `${s.handle}.${/jpe?g/i.test(s.mime) ? 'jpg' : 'png'}`;
}

/** Newest first; ties (same millisecond) keep a stable order by id. */
export function byNewest(a: ScreenshotMeta, b: ScreenshotMeta): number {
  return b.created_at_ms - a.created_at_ms || (a.id < b.id ? 1 : a.id > b.id ? -1 : 0);
}

/** Add or replace `incoming` (by id) and return the list newest first, at most `limit` long. */
export function mergeShots(list: readonly ScreenshotMeta[], incoming: readonly ScreenshotMeta[], limit = LIST_LIMIT): ScreenshotMeta[] {
  const byId = new Map<string, ScreenshotMeta>();
  for (const s of list) byId.set(s.id, s);
  for (const s of incoming) byId.set(s.id, s);
  return [...byId.values()].sort(byNewest).slice(0, limit);
}

/** Drop deleted ids. */
export const withoutShots = (list: readonly ScreenshotMeta[], ids: ReadonlySet<string>): ScreenshotMeta[] => list.filter((s) => !ids.has(s.id));

/** Ids of screenshots newer than the last time the tab was open (null = never recorded: none). */
export function unreadSince(list: readonly ScreenshotMeta[], seenMs: number | null): Set<string> {
  const out = new Set<string>();
  if (seenMs === null) return out;
  for (const s of list) if (s.created_at_ms > seenMs) out.add(s.id);
  return out;
}

/** The moment to record as "seen": the newest screenshot, else `fallbackMs`. */
export const seenMark = (list: readonly ScreenshotMeta[], fallbackMs: number): number => list.reduce((m, s) => Math.max(m, s.created_at_ms), fallbackMs);

/** What a `screenshot.captured` event tells the list, or null for any other event. */
export interface CapturedHint {
  /** The screenshot record's id, when the event carries it. */
  id: string | null;
  workspace: string | null;
  pane: string | null;
  handle: string | null;
  caption: string | null;
}

export function capturedHint(e: Pick<AppEvent, 'type' | 'subject' | 'data'>): CapturedHint | null {
  if (e.type !== 'screenshot.captured') return null;
  const str = (v: unknown): string | null => (typeof v === 'string' && v ? v : null);
  return {
    id: str(e.data.id),
    workspace: str(e.subject.workspace),
    pane: str(e.subject.pane) ?? str(e.data.pane),
    handle: str(e.data.handle),
    caption: str(e.data.caption),
  };
}

/** Ids a `screenshot.deleted` event removed. */
export function deletedIds(e: Pick<AppEvent, 'type' | 'data'>): string[] | null {
  if (e.type !== 'screenshot.deleted') return null;
  const ids = e.data.ids;
  return Array.isArray(ids) ? ids.filter((x): x is string => typeof x === 'string') : [];
}

/** The next index in a viewer over `count` items, clamped (no wrap-around). */
export const stepIndex = (i: number, delta: number, count: number): number => (count <= 0 ? 0 : Math.min(count - 1, Math.max(0, i + delta)));

/** A least-recently-used cache bounded by the number of entries and their total size. */
export class ByteLru<V> {
  private readonly map = new Map<string, { v: V; size: number }>();
  private total = 0;
  constructor(
    private readonly maxEntries: number,
    private readonly maxBytes: number,
  ) {}
  get(key: string): V | undefined {
    const e = this.map.get(key);
    if (!e) return undefined;
    this.map.delete(key);
    this.map.set(key, e);
    return e.v;
  }
  set(key: string, v: V, size: number): void {
    const old = this.map.get(key);
    if (old) this.total -= old.size;
    this.map.delete(key);
    this.map.set(key, { v, size });
    this.total += size;
    while (this.map.size > 1 && (this.map.size > this.maxEntries || this.total > this.maxBytes)) {
      const k = this.map.keys().next().value as string;
      this.total -= this.map.get(k)!.size;
      this.map.delete(k);
    }
  }
  get size(): number {
    return this.map.size;
  }
  get bytes(): number {
    return this.total;
  }
}
