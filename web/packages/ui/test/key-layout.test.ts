import { describe, expect, test } from 'bun:test';
import { NO_MODS } from '../src/lib/keys';
import {
  DEFAULT_LAYOUT, FUNCTION_KEYS, PRESET_IDS, addKey, buildStep, cleanKey, decodeLayout, encodeLayout, isDangerous, isRepeatable,
  moveKey, packRows, padLabel, preset, removeKey, resolvePad, setSpan, type KeyLayout,
} from '../src/lib/key-layout';
import { HoldRepeater, REPEAT_CAP_MS, REPEAT_DELAY_MS, REPEAT_INTERVAL_MS, type RepeatTimers } from '../src/lib/hold-repeat';
import { insertAtCaret } from '../src/lib/voice-insert';
import { DEFAULT_PREFS, parsePrefs } from '../src/lib/prefs';

describe('key layouts', () => {
  test('presets are valid and survive a code round trip', () => {
    for (const id of PRESET_IDS) {
      const l = preset(id);
      expect(l.keys.length).toBeGreaterThan(0);
      for (const x of l.keys) expect(cleanKey(x)).not.toBeNull();
      const r = decodeLayout(encodeLayout(l));
      expect(r).toEqual({ ok: true, layout: l });
    }
    expect(FUNCTION_KEYS.map((x) => x.steps[0])).toEqual(Array.from({ length: 12 }, (_, i) => `f${i + 1}`));
  });
  test('the code is short, url safe and keeps non-ASCII labels', () => {
    const l: KeyLayout = { name: 'Mine', keys: [{ steps: ['shift+tab'], label: '⇧⇥', span: 2 }] };
    const code = encodeLayout(l);
    expect(code).toMatch(/^vk1\.[A-Za-z0-9_-]+$/);
    expect(decodeLayout(code)).toEqual({ ok: true, layout: l });
    expect(encodeLayout(DEFAULT_LAYOUT).length).toBeLessThan(400);
  });
  test('decoding rejects bad input without throwing', () => {
    const bad = (s: string) => decodeLayout(s);
    expect(bad('')).toEqual({ ok: false, error: 'empty' });
    expect(bad('hello')).toEqual({ ok: false, error: 'format' });
    expect(bad('vk1.!!!')).toEqual({ ok: false, error: 'format' });
    expect(bad('vk1.AAAA')).toEqual({ ok: false, error: 'format' });
    expect(bad('vk1.' + 'A'.repeat(7000))).toEqual({ ok: false, error: 'too_long' });
    const enc = (v: unknown) => 'vk1.' + btoa(JSON.stringify(v)).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
    expect(bad(enc({ n: 'x', k: [[1, '', 'notakey']] }))).toEqual({ ok: false, error: 'invalid' });
    expect(bad(enc({ n: 'x', k: [[4, '', 'esc']] }))).toEqual({ ok: false, error: 'invalid' });
    expect(bad(enc({ n: 'x', k: [[1, '', 'a', 'b', 'c', 'd', 'e']] }))).toEqual({ ok: false, error: 'invalid' });
    expect(bad(enc({ n: 'x', k: [] }))).toEqual({ ok: false, error: 'invalid' });
    expect(bad(enc({ n: 'x', k: Array.from({ length: 61 }, () => [1, '', 'a']) }))).toEqual({ ok: false, error: 'invalid' });
    expect(bad(enc({ n: 'x', k: [[1, 5, 'a']] }))).toEqual({ ok: false, error: 'invalid' });
    expect(bad(enc({ n: 'ok', k: [[1, '', 'a']] })).ok).toBe(true);
  });
  test('dangerous keys need a second tap', () => {
    expect(isDangerous({ steps: ['ctrl+d'] })).toBe(true);
    expect(isDangerous({ steps: ['ctrl+z'] })).toBe(true);
    expect(isDangerous({ steps: ['ctrl+c'] })).toBe(false);
    expect(isDangerous({ steps: ['a', 'ctrl+c'] })).toBe(true);
    expect(isDangerous({ steps: ['esc', 'enter'] })).toBe(false);
  });
  test('only plain arrows repeat', () => {
    expect(isRepeatable({ steps: ['up'] })).toBe(true);
    expect(isRepeatable({ steps: ['ctrl+up'] })).toBe(false);
    expect(isRepeatable({ steps: ['up', 'up'] })).toBe(false);
    expect(isRepeatable({ steps: ['enter'] })).toBe(false);
  });
  test('sticky modifiers apply to a single plain key only', () => {
    const ctrlOnce = { ...NO_MODS, ctrl: 'once' as const };
    expect(resolvePad({ steps: ['c'] }, ctrlOnce)).toEqual({ keys: ['ctrl+c'], mods: NO_MODS });
    expect(resolvePad({ steps: ['shift+tab'] }, ctrlOnce)).toEqual({ keys: ['shift+tab'], mods: ctrlOnce });
    expect(resolvePad({ steps: [':', 'w'] }, ctrlOnce)).toEqual({ keys: [':', 'w'], mods: ctrlOnce });
  });
  test('editing helpers', () => {
    const keys = preset('default').keys;
    expect(moveKey(keys, 0, 2).slice(0, 3).map((x) => x.steps[0])).toEqual(['tab', 'shift+tab', 'esc']);
    expect(moveKey(keys, 0, 99)).toEqual(keys);
    expect(removeKey(keys, 0)).toHaveLength(keys.length - 1);
    expect(setSpan(keys, 1, 3)[1]!.span).toBe(3);
    expect(addKey(keys, { steps: ['f5'], label: '', span: 1 })).toHaveLength(keys.length + 1);
    expect(addKey(keys, { steps: ['nope'], label: '', span: 1 })).toHaveLength(keys.length);
    expect(buildStep({ ctrl: true }, 'C')).toBe('ctrl+C');
    expect(buildStep({ ctrl: true, shift: true }, 'Up')).toBe('ctrl+shift+up');
    expect(buildStep({}, 'f5')).toBe('f5');
    expect(buildStep({}, 'banana')).toBeNull();
    expect(buildStep({}, '')).toBeNull();
    expect(padLabel({ steps: [':', 'w', 'enter'], label: '', span: 1 })).toBe(': W ⏎');
  });
  test('rows pack by span and mirror for left-hand use', () => {
    const keys = [1, 2, 3, 3, 1].map((span) => ({ steps: ['a'], label: '', span: span as 1 | 2 | 3 }));
    const rows = packRows(keys);
    expect(rows.map((r) => r.map((p) => [p.start, p.span]))).toEqual([[[1, 1], [2, 2], [4, 3]], [[1, 3], [4, 1]]]);
    const m = packRows(keys, true);
    expect(m[0]!.map((p) => [p.index, p.start])).toEqual([[2, 1], [1, 4], [0, 6]]);
    expect(m[1]!.map((p) => [p.index, p.start])).toEqual([[4, 3], [3, 4]]);
    for (const r of m) for (const p of r) expect(p.start + p.span - 1).toBeLessThanOrEqual(6);
  });
  test('prefs keep a valid layout and drop a broken one', () => {
    expect(DEFAULT_PREFS.keyLayout).toBeNull();
    const l = preset('vim');
    expect(parsePrefs(JSON.stringify({ keyLayout: l })).keyLayout).toEqual(l);
    expect(parsePrefs(JSON.stringify({ keyLayout: { name: 'x', keys: [{ steps: ['zzz'], label: '', span: 1 }] } })).keyLayout).toBeNull();
    expect(parsePrefs(JSON.stringify({ leftHand: true })).leftHand).toBe(true);
    expect(parsePrefs(JSON.stringify({ leftHand: 'yes' })).leftHand).toBe(false);
  });
});

function fakeTimers() {
  let now = 0;
  let seq = 0;
  const q = new Map<number, { at: number; fn: () => void }>();
  const timers: RepeatTimers = {
    set: (fn, ms) => {
      q.set(++seq, { at: now + ms, fn });
      return seq;
    },
    clear: (h) => void q.delete(h as number),
    now: () => now,
  };
  const advance = async (ms: number) => {
    const end = now + ms;
    for (;;) {
      const next = [...q.entries()].filter(([, v]) => v.at <= end).sort((a, b) => a[1].at - b[1].at)[0];
      if (!next) break;
      q.delete(next[0]);
      now = next[1].at;
      next[1].fn();
      await Promise.resolve();
      await Promise.resolve();
    }
    now = end;
  };
  return { timers, advance, pending: () => q.size };
}

describe('hold to repeat', () => {
  test('sends once, then repeats after the delay at the interval', async () => {
    const f = fakeTimers();
    let n = 0;
    const r = new HoldRepeater(async () => (n++, true), f.timers);
    r.start();
    expect(n).toBe(1);
    await f.advance(REPEAT_DELAY_MS - 1);
    expect(n).toBe(1);
    await f.advance(1);
    expect(n).toBe(2);
    await f.advance(REPEAT_INTERVAL_MS * 3);
    expect(n).toBe(5);
    r.stop();
    await f.advance(1000);
    expect(n).toBe(5);
    expect(f.pending()).toBe(0);
  });
  test('stops at the cap', async () => {
    const f = fakeTimers();
    let n = 0;
    const r = new HoldRepeater(async () => (n++, true), f.timers);
    r.start();
    await f.advance(REPEAT_CAP_MS + 1000);
    const at = n;
    expect(r.active).toBe(false);
    await f.advance(1000);
    expect(n).toBe(at);
    expect(at).toBeLessThanOrEqual(Math.ceil((REPEAT_CAP_MS - REPEAT_DELAY_MS) / REPEAT_INTERVAL_MS) + 2);
  });
  test('never has two sends in flight and stops after a failure', async () => {
    const f = fakeTimers();
    let flying = 0;
    let max = 0;
    let n = 0;
    const gates: (() => void)[] = [];
    const r = new HoldRepeater(
      () =>
        new Promise<boolean>((res) => {
          n++;
          flying++;
          max = Math.max(max, flying);
          gates.push(() => {
            flying--;
            res(true);
          });
        }),
      f.timers,
    );
    r.start();
    await f.advance(1000);
    expect(n).toBe(1);
    expect(max).toBe(1);
    gates.shift()!();
    await Promise.resolve();
    await Promise.resolve();
    await f.advance(REPEAT_INTERVAL_MS);
    expect(n).toBe(2);
    r.stop();
    const f2 = fakeTimers();
    let m = 0;
    const bad = new HoldRepeater(async () => (m++, false), f2.timers);
    bad.start();
    await f2.advance(2000);
    expect(m).toBe(1);
  });
});

describe('voice insertion', () => {
  test('inserts at the caret with word spacing', () => {
    expect(insertAtCaret('hello world', 5, 5, 'big')).toEqual({ text: 'hello big world', caret: 9 });
    expect(insertAtCaret('', 0, 0, ' hi ')).toEqual({ text: 'hi', caret: 2 });
    expect(insertAtCaret('abc ', 4, 4, 'def')).toEqual({ text: 'abc def', caret: 7 });
    expect(insertAtCaret('one two', 4, 7, 'three')).toEqual({ text: 'one three', caret: 9 });
    expect(insertAtCaret('abc', 99, 99, 'x')).toEqual({ text: 'abc x', caret: 5 });
    expect(insertAtCaret('abc', 1, 1, '  ')).toEqual({ text: 'abc', caret: 1 });
  });
});
