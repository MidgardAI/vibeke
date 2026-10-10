import { describe, expect, test } from 'bun:test';
import type { ScreenshotMeta } from '@vibeke/core';
import { ByteLru, listParams, thumbEdge, captionOf, capturedHint, deletedIds, mergeShots, saveName, seenMark, stepIndex, tooBigToInline, unreadSince, INLINE_MAX_BYTES } from '../src/lib/screenshots';

const shot = (id: string, at: number, extra: Partial<ScreenshotMeta> = {}): ScreenshotMeta => ({ id, handle: `s${id}`, blob: id, mime: 'image/png', width: 10, height: 10, bytes: 100, created_at_ms: at, label: '', ...extra });

describe('screenshot list', () => {
  test('merge keeps newest first, replaces by id and caps the length', () => {
    const a = [shot('1', 10), shot('2', 20)];
    const out = mergeShots(a, [shot('3', 30), shot('1', 10, { caption: 'x' })]);
    expect(out.map((s) => s.id)).toEqual(['3', '2', '1']);
    expect(out[2]!.caption).toBe('x');
    expect(mergeShots(a, [shot('3', 30)], 2).map((s) => s.id)).toEqual(['3', '2']);
  });
  test('unread counts what is newer than the last seen time', () => {
    const l = [shot('1', 10), shot('2', 20), shot('3', 30)];
    expect([...unreadSince(l, 15)].sort()).toEqual(['2', '3']);
    expect(unreadSince(l, null).size).toBe(0);
    expect(seenMark(l, 5)).toBe(30);
    expect(seenMark([], 5)).toBe(5);
  });
  test('caption falls back to file name, title, label, handle', () => {
    expect(captionOf(shot('1', 1, { caption: ' Login page ', source_name: 'a.png' }))).toBe('Login page');
    expect(captionOf(shot('1', 1, { source_name: 'a.png' }))).toBe('a.png');
    expect(captionOf(shot('1', 1, { title: 'Home', label: 'box' }))).toBe('Home');
    expect(captionOf(shot('1', 1, { label: 'box · headless' }))).toBe('box · headless');
    expect(captionOf(shot('7', 1))).toBe('s7');
  });
  test('save names, size limit and stepping', () => {
    expect(saveName(shot('1', 1, { source_name: 'dir/a.png' }))).toBe('dir_a.png');
    expect(saveName(shot('1', 1))).toBe('s1.png');
    expect(tooBigToInline(shot('1', 1, { bytes: INLINE_MAX_BYTES + 1 }))).toBe(true);
    expect(tooBigToInline(shot('1', 1, { bytes: INLINE_MAX_BYTES }))).toBe(false);
    expect(stepIndex(0, -1, 3)).toBe(0);
    expect(stepIndex(2, 1, 3)).toBe(2);
    expect(stepIndex(1, 1, 3)).toBe(2);
  });
  test('events', () => {
    const h = capturedHint({ type: 'screenshot.captured', subject: { workspace: 'w1', pane: 'p1' }, data: { id: 'x', handle: 's1', caption: 'c' } });
    expect(h).toEqual({ id: 'x', workspace: 'w1', pane: 'p1', handle: 's1', caption: 'c' });
    expect(capturedHint({ type: 'pane.closed', subject: {}, data: {} })).toBeNull();
    expect(deletedIds({ type: 'screenshot.deleted', data: { ids: ['a', 1] } })).toEqual(['a']);
  });
  test('byte LRU evicts the oldest by count and by size', () => {
    const c = new ByteLru<string>(2, 100);
    c.set('a', 'A', 10);
    c.set('b', 'B', 10);
    c.get('a');
    c.set('c', 'C', 10);
    expect(c.get('b')).toBeUndefined();
    expect(c.get('a')).toBe('A');
    c.set('d', 'D', 95);
    expect(c.size).toBe(1);
  });
  test('list params: a pane-only share asks for its pane, others for the workspace', () => {
    expect(listParams({ pane: 'p1' }, 'w1')).toEqual({ pane: 'p1', limit: 100 });
    expect(listParams({ workspace: 'w1' }, 'w1')).toEqual({ workspace: 'w1', limit: 100 });
    expect(listParams({ workspace: 'w1', pane: 'p1' }, 'w1')).toEqual({ workspace: 'w1', limit: 100 });
    expect(listParams(null, 'w2')).toEqual({ workspace: 'w2', limit: 100 });
  });
  test('thumbnail edge doubles on high-DPR screens and stays within 1024', () => {
    expect(thumbEdge(1)).toBe(384);
    expect(thumbEdge(2)).toBe(768);
    expect(thumbEdge(4)).toBeLessThanOrEqual(1024);
  });
});
