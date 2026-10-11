import { describe, expect, test } from 'bun:test';
import { RpcError } from '@vibeke/core';
import { RESEND_WINDOW_MS, ResendMemory, isPaneMoving } from '../src/lib/resend';

describe('resending after a lost answer', () => {
  test('the same text to the same place repeats the op_id', () => {
    const m = new ResendMemory();
    expect(m.opIdFor('a', 0)).toBeUndefined();
    m.remember('a', 'op-1', 0);
    expect(m.opIdFor('a', 1000)).toBe('op-1');
    expect(m.opIdFor('b', 1000)).toBeUndefined(); // other text or pane: a new call
    expect(m.opIdFor('a', RESEND_WINDOW_MS + 1)).toBeUndefined();
    m.settle('a');
    expect(m.opIdFor('a', 1000)).toBeUndefined();
  });
});

describe('pane input during a cloud move', () => {
  test('the pane_moving refusal is recognised', () => {
    expect(isPaneMoving(new RpcError('pane.send_text', { code: -32000, message: 'pane_moving: this agent is moving to or from a cloud box' }))).toBe(true);
    expect(isPaneMoving(new RpcError('pane.send_text', { code: -32000, message: 'x', data: { kind: 'conflict', details: { reason: 'pane_moving' } } }))).toBe(true);
    expect(isPaneMoving(new Error('other'))).toBe(false);
    expect(isPaneMoving(null)).toBe(false);
  });
});
