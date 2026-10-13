// WebSocket client framing (RFC 6455) over a tunnel stream (spec 20 §3.6). Service workers
// cannot catch WebSockets, so the review controller speaks the protocol itself: an HTTP upgrade
// (http.ts), then masked client frames out and unmasked server frames in.

import { sha1 } from '@noble/hashes/legacy.js';
import * as b64 from './b64';
import { concat } from './http';

export const OP = { cont: 0, text: 1, binary: 2, close: 8, ping: 9, pong: 10 } as const;

/** Largest message the controller accepts from the dev server. */
export const MAX_WS_MESSAGE = 16 * 1024 * 1024;

/** Standard base64 (with padding), as the handshake headers use. */
function stdB64(bytes: Uint8Array): string {
  const u = b64.encode(bytes).replace(/-/g, '+').replace(/_/g, '/');
  return u + '='.repeat((4 - (u.length % 4)) % 4);
}

/** A fresh `Sec-WebSocket-Key`. */
export const wsKey = (random: (n: number) => Uint8Array): string => stdB64(random(16));

/** The `Sec-WebSocket-Accept` a server must answer for `key`. */
export function wsAccept(key: string): string {
  return stdB64(sha1(new TextEncoder().encode(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`)));
}

/** One masked client frame. */
export function encodeFrame(opcode: number, payload: Uint8Array, mask: Uint8Array, fin = true): Uint8Array {
  if (mask.length !== 4) throw new Error('ws: mask must be 4 bytes');
  const n = payload.length;
  const headLen = 2 + (n < 126 ? 0 : n < 65536 ? 2 : 8) + 4;
  const out = new Uint8Array(headLen + n);
  out[0] = (fin ? 0x80 : 0) | (opcode & 0x0f);
  let i = 2;
  if (n < 126) out[1] = 0x80 | n;
  else if (n < 65536) {
    out[1] = 0x80 | 126;
    out[2] = n >> 8;
    out[3] = n & 0xff;
    i = 4;
  } else {
    out[1] = 0x80 | 127;
    const dv = new DataView(out.buffer);
    dv.setUint32(2, Math.floor(n / 2 ** 32));
    dv.setUint32(6, n >>> 0);
    i = 10;
  }
  out.set(mask, i);
  i += 4;
  for (let j = 0; j < n; j++) out[i + j] = payload[j]! ^ mask[j & 3]!;
  return out;
}

/** A close frame payload: 2-byte code plus UTF-8 reason (≤ 123 bytes). */
export function closePayload(code?: number, reason = ''): Uint8Array {
  if (code === undefined) return new Uint8Array(0);
  const r = new TextEncoder().encode(reason).subarray(0, 123);
  const out = new Uint8Array(2 + r.length);
  out[0] = code >> 8;
  out[1] = code & 0xff;
  out.set(r, 2);
  return out;
}

export type WsEvent =
  | { type: 'text'; data: string }
  | { type: 'binary'; data: Uint8Array }
  | { type: 'ping'; data: Uint8Array }
  | { type: 'pong'; data: Uint8Array }
  | { type: 'close'; code: number; reason: string };

export class WsProtocolError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'WsProtocolError';
  }
}

/** Incremental reader for server frames (unmasked), reassembling fragmented messages. */
export class FrameReader {
  private buf: Uint8Array = new Uint8Array(0);
  private parts: Uint8Array[] = [];
  private partsLen = 0;
  private partOp = 0;

  push(bytes: Uint8Array): WsEvent[] {
    this.buf = concat(this.buf, bytes);
    const out: WsEvent[] = [];
    for (;;) {
      if (this.buf.length < 2) return out;
      const b0 = this.buf[0]!;
      const b1 = this.buf[1]!;
      if (b0 & 0x70) throw new WsProtocolError('reserved bits set');
      if (b1 & 0x80) throw new WsProtocolError('masked server frame');
      const fin = (b0 & 0x80) !== 0;
      const op = b0 & 0x0f;
      let len = b1 & 0x7f;
      let at = 2;
      if (len === 126) {
        if (this.buf.length < 4) return out;
        len = (this.buf[2]! << 8) | this.buf[3]!;
        at = 4;
      } else if (len === 127) {
        if (this.buf.length < 10) return out;
        const dv = new DataView(this.buf.buffer, this.buf.byteOffset, 10);
        const hi = dv.getUint32(2);
        if (hi > 0) throw new WsProtocolError('frame too large');
        len = dv.getUint32(6);
        at = 10;
      }
      if (len > MAX_WS_MESSAGE) throw new WsProtocolError('frame too large');
      if (this.buf.length < at + len) return out;
      const payload = this.buf.slice(at, at + len);
      this.buf = this.buf.subarray(at + len);
      if (op >= 8) {
        if (!fin || len > 125) throw new WsProtocolError('bad control frame');
        if (op === OP.close) {
          const code = len >= 2 ? (payload[0]! << 8) | payload[1]! : 1005;
          const reason = len > 2 ? new TextDecoder().decode(payload.subarray(2)) : '';
          out.push({ type: 'close', code, reason });
        } else if (op === OP.ping) out.push({ type: 'ping', data: payload });
        else if (op === OP.pong) out.push({ type: 'pong', data: payload });
        else throw new WsProtocolError('unknown control opcode');
        continue;
      }
      if (op === OP.cont) {
        if (!this.partOp) throw new WsProtocolError('unexpected continuation');
      } else if (op === OP.text || op === OP.binary) {
        if (this.partOp) throw new WsProtocolError('new message inside a fragmented one');
        this.partOp = op;
      } else throw new WsProtocolError('unknown opcode');
      this.parts.push(payload);
      this.partsLen += payload.length;
      if (this.partsLen > MAX_WS_MESSAGE) throw new WsProtocolError('message too large');
      if (!fin) continue;
      const whole = this.parts.length === 1 ? this.parts[0]! : this.parts.reduce((a, b) => concat(a, b), new Uint8Array(0));
      const kind = this.partOp;
      this.parts = [];
      this.partsLen = 0;
      this.partOp = 0;
      if (kind === OP.text) out.push({ type: 'text', data: new TextDecoder('utf-8', { fatal: false }).decode(whole) });
      else out.push({ type: 'binary', data: whole });
    }
  }
}
