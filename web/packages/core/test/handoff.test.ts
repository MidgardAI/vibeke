import { describe, expect, test } from 'bun:test';
import { isHandoffBusy } from '../src/handoff';
import { RpcError } from '../src/rpc';

describe('handoff send when the agent is working', () => {
  const busy = () => new RpcError('handoff.send', { code: -32000, message: 'the agent is working; wait for it to finish or pass interrupt: true', data: { kind: 'busy' } });

  test('busy kinds', () => {
    expect(isHandoffBusy(busy())).toBe(true);
    expect(isHandoffBusy(new RpcError('handoff.send', { code: -1, message: 'the agent is working', data: { kind: 'conflict' } }))).toBe(true);
    expect(isHandoffBusy(new RpcError('handoff.send', { code: -1, message: 'repo conflict', data: { kind: 'conflict' } }))).toBe(false);
    expect(isHandoffBusy(new Error('busy'))).toBe(false);
  });
});
