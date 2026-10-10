import { expect, test } from 'bun:test';
import { createTuiKeyboard } from '../src/lib/tui-keyboard';
function harness(mac = false, meta = false) {
  const sent: unknown[][] = [];
  const keyboard = createTuiKeyboard((...args) => { sent.push(args); return true; }, { mac, optionAsMeta: () => meta });
  const key = (key: string, extra: Partial<KeyboardEvent> = {}) => keyboard.handle({ key, code: 'KeyA', type: 'keydown', ctrlKey: false, altKey: false, shiftKey: false, metaKey: false, repeat: false, isComposing: false, getModifierState: () => false, preventDefault() {}, ...extra } as KeyboardEvent);
  return { ...keyboard, key, sent };
}
test('layout text, AltGr, dead keys and composition remain with xterm', () => {
  const h = harness();
  for (const key of ['æ', 'ø', 'å', '日本語', 'Dead', 'Process']) expect(h.key(key)).toBe(true);
  expect(h.key('@', { ctrlKey: true, altKey: true })).toBe(true);
  expect(h.key('€', { altKey: true, getModifierState: () => true })).toBe(true);
  expect(h.key('a', { ctrlKey: true, isComposing: true })).toBe(true);
  expect(h.sent).toEqual([]);
});
test('Mac Option text is preserved unless explicitly configured as Meta', () => {
  const h = harness(true); expect(h.key('€', { altKey: true })).toBe(true); expect(h.sent).toEqual([]);
  const meta = harness(true, true); expect(meta.key('a', { altKey: true })).toBe(false); expect(meta.sent[0]).toEqual(['a', 2, false, false]);
});
test('shortcuts preserve their original modifiers on release, without duplicate keypress input', () => {
  const h = harness();
  expect(h.key('Enter', { shiftKey: true, code: 'Enter' })).toBe(false);
  expect(h.key('Enter', { shiftKey: true, code: 'Enter', type: 'keypress' })).toBe(false);
  expect(h.key('Enter', { code: 'Enter', type: 'keyup' })).toBe(false);
  expect(h.sent).toEqual([['Enter', 1, false, false], ['Enter', 1, false, true]]);
  h.key('b', { ctrlKey: true }); h.releaseAll(); h.releaseAll();
  expect(h.sent.slice(2)).toEqual([['b', 4, false, false], ['b', 4, false, true]]);
});
test('browser copy, paste and Command shortcuts are not captured', () => {
  const h = harness();
  for (const key of ['C', 'V']) expect(h.key(key, { ctrlKey: true, shiftKey: true })).toBe(true);
  expect(h.key('c', { metaKey: true })).toBe(true); expect(h.sent).toEqual([]);
});

test('Mac Option-as-Alt translates physical letters, punctuation and dead keys', () => {
  const h = harness(true, true);
  h.key('ƒ', { code: 'KeyF', altKey: true });
  h.key('≥', { code: 'Period', altKey: true });
  h.key('Dead', { code: 'KeyE', altKey: true });
  expect(h.sent).toEqual([['f', 2, false, false], ['.', 2, false, false], ['e', 2, false, false]]);
});

test('Mac Option-as-Alt uses layout-aware letters on AZERTY and QWERTZ', () => {
  const h = harness(true, true);
  h.key('æ', { code: 'KeyQ', keyCode: 65, altKey: true });
  h.key('Ω', { code: 'KeyY', keyCode: 90, altKey: true });
  expect(h.sent).toEqual([['a', 2, false, false], ['z', 2, false, false]]);
});
