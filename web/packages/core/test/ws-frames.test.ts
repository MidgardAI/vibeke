import { describe, expect, test } from 'bun:test';
import { closePayload, encodeFrame, FrameReader, OP, wsAccept, WsProtocolError } from '../src/ws-frames';

const serverFrame = (op: number, payload: Uint8Array, fin = true): Uint8Array => {
  const n = payload.length;
  const head = n < 126 ? [((fin ? 0x80 : 0) | op), n] : [((fin ? 0x80 : 0) | op), 126, n >> 8, n & 0xff];
  return new Uint8Array([...head, ...payload]);
};
const te = new TextEncoder();

describe('websocket frames', () => {
  test('RFC 6455 accept example', () => {
    expect(wsAccept('dGhlIHNhbXBsZSBub25jZQ==')).toBe('s3pPLMBiTxaQ9kYGzzhZRbK+xOo=');
  });

  test('client frames are masked', () => {
    const f = encodeFrame(OP.text, te.encode('Hello'), new Uint8Array([0x37, 0xfa, 0x21, 0x3d]));
    // RFC 6455 §5.7 "A single-frame masked text message"
    expect(Array.from(f)).toEqual([0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58]);
    const big = encodeFrame(OP.binary, new Uint8Array(70000), new Uint8Array(4));
    expect(big[1]).toBe(0x80 | 127);
    expect(big.length).toBe(70000 + 14);
    const mid = encodeFrame(OP.binary, new Uint8Array(300), new Uint8Array(4));
    expect(Array.from(mid.subarray(0, 4))).toEqual([0x82, 0x80 | 126, 1, 44]);
  });

  test('reader: split input, fragments, control frames', () => {
    const r = new FrameReader();
    const all = new Uint8Array([
      ...serverFrame(OP.text, te.encode('Hel'), false),
      ...serverFrame(OP.ping, te.encode('p')),
      ...serverFrame(OP.cont, te.encode('lo')),
      ...serverFrame(OP.binary, new Uint8Array(200).fill(9)),
      ...serverFrame(OP.close, closePayload(1001, 'bye')),
    ]);
    const ev = [];
    for (let i = 0; i < all.length; i += 3) ev.push(...r.push(all.subarray(i, i + 3)));
    expect(ev.map((e) => e.type)).toEqual(['ping', 'text', 'binary', 'close']);
    expect(ev[1]).toEqual({ type: 'text', data: 'Hello' });
    expect((ev[2] as { data: Uint8Array }).data.length).toBe(200);
    expect(ev[3]).toEqual({ type: 'close', code: 1001, reason: 'bye' });
  });

  test('protocol errors', () => {
    expect(() => new FrameReader().push(new Uint8Array([0x81, 0x81, 0, 0, 0, 0, 1]))).toThrow(WsProtocolError);
    expect(() => new FrameReader().push(serverFrame(OP.cont, new Uint8Array(1)))).toThrow('unexpected continuation');
    expect(() => new FrameReader().push(new Uint8Array([0xc1, 0]))).toThrow('reserved');
  });
});
