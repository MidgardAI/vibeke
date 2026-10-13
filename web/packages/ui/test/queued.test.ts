import { describe, expect, test } from 'bun:test';
import { RpcError, type AppEvent } from '@vibeke/core';
import { isQueueEvent, nothingQueued, queuedItems, queueUnsupported } from '../src/lib/queued';

const ev = (type: string, run: string): AppEvent => ({ type, subject: { run } }) as never;
const rpcErr = (kind: string, details: unknown, code = -32009) => new RpcError('agent.send_now', { code, message: kind, data: { kind, details } } as never);

describe('queued messages', () => {
  test('queue and run events of the same run refresh the list', () => {
    expect(isQueueEvent(ev('agent.queued_changed', 'r1'), 'r1')).toBe(true);
    expect(isQueueEvent(ev('agent.turn_started', 'r1'), 'r1')).toBe(true);
    expect(isQueueEvent(ev('agent.queued_changed', 'r2'), 'r1')).toBe(false);
    expect(isQueueEvent(ev('pane.output', 'r1'), 'r1')).toBe(false);
  });

  test('items come oldest first; broken answers show nothing', () => {
    const r = { run: 'r1', items: [{ id: 'b', text: 'second', since_ms: 20, source: 'transcript' }, { id: 'a', text: 'first', since_ms: 10, source: 'transcript' }, { text: 'no id' }] };
    expect(queuedItems(r).map((i) => i.id)).toEqual(['a', 'b']);
    expect(queuedItems(null)).toEqual([]);
    expect(queuedItems({ items: 'x' })).toEqual([]);
  });

  test('errors: nothing queued, and hosts without the method', () => {
    expect(nothingQueued(rpcErr('conflict', { reason: 'nothing_queued' }))).toBe(true);
    expect(nothingQueued(rpcErr('conflict', { reason: 'dialog_open' }))).toBe(false);
    expect(nothingQueued(new Error('x'))).toBe(false);
    expect(queueUnsupported(rpcErr('method_not_found', null, -32601))).toBe(true);
    expect(queueUnsupported(rpcErr('conflict', null))).toBe(false);
  });
});
