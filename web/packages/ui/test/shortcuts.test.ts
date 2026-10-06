import { describe, expect, test } from 'bun:test';
import { fuzzyScore, keyLabel, shortcutFor, type KeyContext, type KeyLike } from '../src/lib/shortcuts';

const k = (key: string, m: Partial<KeyLike> = {}): KeyLike => ({ key, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...m });
const ctx = (c: Partial<KeyContext> = {}): KeyContext => ({ mac: true, typing: false, dialog: false, onControl: false, ...c });

describe('keyboard shortcuts', () => {
  test('list keys', () => {
    expect(shortcutFor(k('j'), ctx())).toEqual({ type: 'next' });
    expect(shortcutFor(k('k'), ctx())).toEqual({ type: 'prev' });
    expect(shortcutFor(k('a'), ctx())).toEqual({ type: 'allow' });
    expect(shortcutFor(k('A', { shiftKey: true }), ctx())).toEqual({ type: 'allowAlways' });
    expect(shortcutFor(k('d'), ctx())).toEqual({ type: 'deny' });
    expect(shortcutFor(k('Enter'), ctx())).toEqual({ type: 'open' });
    expect(shortcutFor(k('Escape'), ctx())).toEqual({ type: 'back' });
    expect(shortcutFor(k('/'), ctx())).toEqual({ type: 'find' });
    expect(shortcutFor(k('?', { shiftKey: true }), ctx())).toEqual({ type: 'help' });
  });
  test('never while typing, in a dialog, or with modifiers; Enter on a button stays native', () => {
    for (const c of [ctx({ typing: true }), ctx({ dialog: true })]) expect(shortcutFor(k('a'), c)).toBeNull();
    expect(shortcutFor(k('a', { altKey: true }), ctx())).toBeNull();
    expect(shortcutFor(k('a', { isComposing: true }), ctx())).toBeNull();
    expect(shortcutFor(k('Enter'), ctx({ onControl: true }))).toBeNull();
  });
  test('a held key never answers: repeats of mutating shortcuts are ignored, movement repeats', () => {
    for (const key of ['a', 'd', 'A', 'Enter']) expect(shortcutFor(k(key, { repeat: true, shiftKey: key === 'A' }), ctx())).toBeNull();
    expect(shortcutFor(k('j', { repeat: true }), ctx())).toEqual({ type: 'next' });
    expect(shortcutFor(k('k', { repeat: true }), ctx())).toEqual({ type: 'prev' });
  });
  test('palette and tabs use ⌘ on Apple platforms, Ctrl elsewhere, even while typing', () => {
    expect(shortcutFor(k('k', { metaKey: true }), ctx({ typing: true }))).toEqual({ type: 'palette' });
    expect(shortcutFor(k('k', { ctrlKey: true }), ctx())).toBeNull();
    expect(shortcutFor(k('k', { ctrlKey: true }), ctx({ mac: false }))).toEqual({ type: 'palette' });
    expect(shortcutFor(k('3', { metaKey: true }), ctx())).toEqual({ type: 'tab', tab: 'focus' });
    expect(shortcutFor(k('5', { metaKey: true }), ctx())).toBeNull();
    expect(keyLabel(true, 'mod+k')).toBe('⌘K');
    expect(keyLabel(false, 'mod+1')).toBe('Ctrl+1');
    expect(keyLabel(true, 'j')).toBe('j');
  });
  test('fuzzy matching prefers prefixes and word starts', () => {
    expect(fuzzyScore('sah', 'samplehub')).toBeGreaterThan(fuzzyScore('sah', 'the samplehub'));
    expect(fuzzyScore('shb', 'samplehub')).toBeGreaterThan(0);
    expect(fuzzyScore('xyz', 'samplehub')).toBe(-1);
    expect(fuzzyScore('', 'anything')).toBe(0);
  });
});
