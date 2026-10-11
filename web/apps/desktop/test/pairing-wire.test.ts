import { describe, expect, test } from 'bun:test';
import { ChannelError, PairingError } from '@vibeke/core';
import { toWire } from '../src/main/engine';
import { fromWire } from '../src/renderer/remote';

describe('pairing errors over IPC', () => {
  test('the relay reason survives the trip, so the pairing screen can name it', () => {
    const cause = new ChannelError('unauthorized', 'relay refused', { closeCode: 4401, remote: { error: 'unauthorized', reason: 'ticket_expired' } });
    const back = fromWire(toWire(new PairingError('channel', 'pairing failed', cause)));
    expect(back).toBeInstanceOf(PairingError);
    const c = (back as PairingError).cause;
    expect(c).toBeInstanceOf(ChannelError);
    expect((c as ChannelError).code).toBe('unauthorized');
    expect((c as ChannelError).reason).toBe('ticket_expired');
    expect((c as ChannelError).closeCode).toBe(4401);
  });
  test('no reason stays no reason', () => {
    const back = fromWire(toWire(new PairingError('channel', 'x', new ChannelError('closed', 'gone'))));
    expect(((back as PairingError).cause as ChannelError).reason).toBeUndefined();
  });
});
