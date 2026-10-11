import { describe, expect, test } from 'bun:test';
import { InputBatcher, MAX_BATCH, inputToEvent, isLoopbackUrl, namedKey, sizeChanged, toViewport, usableSize } from '../src/lib/phone-preview';

describe('pointer to viewport', () => {
  test('scales and clamps', () => {
    const box = { left: 10, top: 20, width: 200, height: 400 };
    const vp = { width: 400, height: 800 };
    expect(toViewport({ x: 110, y: 220 }, box, vp)).toEqual({ x: 200, y: 400 });
    expect(toViewport({ x: 0, y: 0 }, box, vp)).toEqual({ x: 0, y: 0 });
    expect(toViewport({ x: 999, y: 999 }, box, vp)).toEqual({ x: 399, y: 799 });
  });
});

describe('input batching', () => {
  test('a tap is a start and an end', () => {
    const b = new InputBatcher();
    b.down(7, 10, 20);
    b.up(7);
    expect(b.take()).toEqual([
      { type: 'touch', phase: 'start', points: [{ id: 0, x: 10, y: 20 }] },
      { type: 'touch', phase: 'end', points: [] },
    ]);
    expect(b.pending).toBe(false);
  });
  test('moves coalesce into the newest', () => {
    const b = new InputBatcher();
    b.down(1, 0, 0);
    b.move(1, 1, 1);
    b.move(1, 2, 5);
    b.move(1, 2, 5);
    const ev = b.take();
    expect(ev.length).toBe(2);
    expect(ev[1]).toEqual({ type: 'touch', phase: 'move', points: [{ id: 0, x: 2, y: 5 }] });
  });
  test('two fingers get touch ids 0 and 1 and the freed id is reused', () => {
    const b = new InputBatcher();
    b.down(11, 1, 1);
    b.down(12, 2, 2);
    b.take();
    b.up(11);
    expect(b.take()).toEqual([{ type: 'touch', phase: 'end', points: [{ id: 1, x: 2, y: 2 }] }]);
    b.down(13, 3, 3);
    expect(b.take()[0]).toEqual({
      type: 'touch',
      phase: 'start',
      points: [
        { id: 0, x: 3, y: 3 },
        { id: 1, x: 2, y: 2 },
      ],
    });
    expect(b.touching).toBe(true);
  });
  test('cancel, unknown pointers and repeated down are handled', () => {
    const b = new InputBatcher();
    b.up(5);
    b.move(5, 1, 1);
    b.down(5, 1, 1);
    b.down(5, 9, 9);
    b.up(5, true);
    expect(b.take().map((e) => (e.type === 'touch' ? e.phase : e.type))).toEqual(['start', 'cancel']);
  });
  test('wheel adds up and the last resize wins', () => {
    const b = new InputBatcher();
    b.wheel(1, 2, 0, 10);
    b.wheel(1, 2, 0, 5);
    b.push({ type: 'resize', width: 100, height: 100 });
    b.push({ type: 'text', text: 'a' });
    b.push({ type: 'resize', width: 100, height: 50 });
    expect(b.take()).toEqual([
      { type: 'wheel', x: 1, y: 2, dx: 0, dy: 15 },
      { type: 'text', text: 'a' },
      { type: 'resize', width: 100, height: 50 },
    ]);
  });
  test('a batch holds at most 64 events and keeps the rest', () => {
    const b = new InputBatcher();
    for (let i = 0; i < 70; i++) b.push({ type: 'key', key: 'a' });
    expect(b.take().length).toBe(MAX_BATCH);
    expect(b.pending).toBe(true);
    expect(b.take().length).toBe(6);
  });
});

describe('keyboard input', () => {
  test('beforeinput types map to events', () => {
    expect(inputToEvent('insertText', 'x')).toEqual({ type: 'text', text: 'x' });
    expect(inputToEvent('insertText', null)).toBeNull();
    expect(inputToEvent('insertLineBreak', null)).toEqual({ type: 'key', key: 'Enter' });
    expect(inputToEvent('deleteContentBackward', null)).toEqual({ type: 'key', key: 'Backspace' });
    expect(inputToEvent('historyUndo', null)).toBeNull();
  });
  test('named keys', () => {
    expect(namedKey('Enter')).toBe('Enter');
    expect(namedKey('a')).toBeNull();
  });
});

describe('sizes', () => {
  test('the keyboard cuts the usable height', () => {
    expect(usableSize({ top: 100, width: 390.4, height: 700 }, null)).toEqual({ width: 390, height: 700 });
    expect(usableSize({ top: 100, width: 390, height: 700 }, { height: 500, offsetTop: 0 })).toEqual({ width: 390, height: 400 });
    expect(usableSize({ top: 100, width: 390, height: 700 }, { height: 50, offsetTop: 0 })).toEqual({ width: 390, height: 0 });
  });
  test('jitter is ignored', () => {
    expect(sizeChanged({ width: 390, height: 700 }, { width: 391, height: 699 })).toBe(false);
    expect(sizeChanged({ width: 390, height: 700 }, { width: 390, height: 400 })).toBe(true);
  });
});

describe('loopback URLs', () => {
  test('loopback hosts', () => {
    for (const u of ['http://localhost:5173/', 'http://app.localhost/', 'http://127.0.0.1:3000', 'http://127.8.9.1/', 'http://[::1]:8080/', 'http://0.0.0.0:80/', 'http://LOCALHOST./']) {
      expect(isLoopbackUrl(u)).toBe(true);
    }
  });
  test('other hosts', () => {
    for (const u of ['https://example.com/', 'http://10.0.0.5:3000/', 'http://128.0.0.1/', 'http://localhost.example.com/', 'not a url']) {
      expect(isLoopbackUrl(u)).toBe(false);
    }
  });
});
