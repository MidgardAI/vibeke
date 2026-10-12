import { describe, expect, test } from 'bun:test';
import type { AnnotationDraft, AnnotationNote } from '@vibeke/core';
import { dragged, isEditable, keepThumbs, noteTitle, pickParams, regionStyle, sendOutcome } from '../src/lib/annotations';

const note = (n: number, extra: Partial<AnnotationNote> = {}): AnnotationNote => ({ n, kind: 'element', note: '', ...extra });
const draft = (notes: AnnotationNote[], state: AnnotationDraft['state'] = 'open'): AnnotationDraft => ({ id: 'd1', target: { preview: 'p1' }, url: 'http://localhost/', notes, state, updated_at_ms: 1 });

describe('pick gestures', () => {
  test('a tap picks the point it ended on', () => {
    expect(pickParams('s1', { x: 10.4, y: 20 }, { x: 10.6, y: 20.2 }, false)).toEqual({ session: 's1', x: 11, y: 20 });
  });

  test('a region drag becomes a rectangle from the top left, whatever direction it went', () => {
    expect(pickParams('s1', { x: 100, y: 80 }, { x: 40, y: 30 }, true)).toEqual({ session: 's1', x: 40, y: 30, w: 60, h: 50 });
  });

  test('a long press that barely moved is still a tap', () => {
    expect(pickParams('s1', { x: 5, y: 5 }, { x: 8, y: 7 }, true)).toEqual({ session: 's1', x: 8, y: 7 });
  });

  test('dragged needs 8 px on either axis', () => {
    expect(dragged({ x: 0, y: 0 }, { x: 7, y: 7 })).toBe(false);
    expect(dragged({ x: 0, y: 0 }, { x: 0, y: -8 })).toBe(true);
  });

  test('the drag rectangle is drawn in percent of the viewport and clamped', () => {
    expect(regionStyle({ a: { x: 50, y: 100 }, b: { x: 150, y: 50 } }, { width: 200, height: 400 })).toEqual({ left: '25%', top: '12.5%', width: '50%', height: '12.5%' });
    expect(regionStyle({ a: { x: -10, y: 0 }, b: { x: 500, y: 0 } }, { width: 200, height: 400 }).left).toBe('0%');
  });
});

describe('note list', () => {
  test('titles prefer the name, then the text, then the selector', () => {
    expect(noteTitle(note(1, { name: 'Buy now', text: 'x', selector: '#b' }))).toBe('Buy now');
    expect(noteTitle(note(1, { text: 'Hello   there\nworld' }))).toBe('Hello there world');
    expect(noteTitle(note(1, { selector: 'main > p' }))).toBe('main > p');
    expect(noteTitle(note(1, { text: 'x'.repeat(100) })).length).toBe(58);
    expect(noteTitle(note(2, { kind: 'region' }))).toBe('Area');
  });

  test('screenshots are kept only for notes that still exist', () => {
    const thumbs = new Map([
      [1, 'a'],
      [2, 'b'],
    ]);
    expect([...keepThumbs(thumbs, draft([note(2), note(3)]))]).toEqual([[2, 'b']]);
    expect(keepThumbs(thumbs, null).size).toBe(0);
  });

  test('a sent draft is history, not editable', () => {
    expect(isEditable(null)).toBe(false);
    expect(isEditable(draft([], 'delivered'))).toBe(false);
    expect(isEditable(draft([], 'queued'))).toBe(true);
    expect(isEditable(draft([], 'sending'))).toBe(false);
    expect(isEditable(draft([], 'unconfirmed'))).toBe(true);
    expect(isEditable(draft([]))).toBe(true);
  });

  test('anything but delivered counts as queued', () => {
    expect(sendOutcome('delivered')).toBe('delivered');
    expect(sendOutcome('queued')).toBe('queued');
    expect(sendOutcome(undefined)).toBe('queued');
    expect(sendOutcome('sending')).toBe('sending');
    expect(sendOutcome('unconfirmed')).toBe('unconfirmed');
    expect(sendOutcome('open')).toBe('queued');
  });
});
