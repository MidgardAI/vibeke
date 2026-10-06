// Browser WebSocket → core Socket (spec 16 §5): text stays string, binary arrives as Uint8Array.
// Socket errors are reported through onclose (code 1006 when the browser gives none).

import type { Socket, SocketState } from '@vibeke/core';

export interface WebSocketLike {
  readonly readyState: number;
  binaryType: string;
  onopen: ((ev: unknown) => void) | null;
  onmessage: ((ev: { data: unknown }) => void) | null;
  onclose: ((ev: { code: number; reason: string }) => void) | null;
  onerror: ((ev: unknown) => void) | null;
  send(data: string | ArrayBufferLike | ArrayBufferView): void;
  close(code?: number, reason?: string): void;
}

export class WsSocket implements Socket {
  state: SocketState = 'connecting';
  onopen: (() => void) | null = null;
  onmessage: ((data: string | Uint8Array) => void) | null = null;
  onclose: ((code: number, reason: string) => void) | null = null;
  private closedFired = false;

  constructor(private readonly ws: WebSocketLike) {
    ws.binaryType = 'arraybuffer';
    ws.onopen = () => {
      if (this.state !== 'connecting') return;
      this.state = 'open';
      this.onopen?.();
    };
    ws.onmessage = (ev) => {
      if (this.state !== 'open') return;
      const d = ev.data;
      if (typeof d === 'string') this.onmessage?.(d);
      else if (d instanceof ArrayBuffer) this.onmessage?.(new Uint8Array(d));
      else if (ArrayBuffer.isView(d)) this.onmessage?.(new Uint8Array(d.buffer, d.byteOffset, d.byteLength));
    };
    ws.onerror = () => {
      // Followed by onclose in browsers; make sure we report once even if it is not.
      queueMicrotask(() => this.fireClose(1006, 'error'));
    };
    ws.onclose = (ev) => this.fireClose(ev.code || 1006, ev.reason || '');
  }

  private fireClose(code: number, reason: string): void {
    if (this.closedFired) return;
    this.closedFired = true;
    this.state = 'closed';
    this.onclose?.(code, reason);
  }

  send(data: string | Uint8Array): void {
    if (this.state !== 'open') throw new Error(`send on ${this.state} socket`);
    this.ws.send(data);
  }

  close(code = 1000, reason = ''): void {
    if (this.state === 'closed') return;
    try {
      this.ws.close(code >= 4000 || code === 1000 ? code : 1000, reason);
    } catch {
      this.ws.close();
    }
    this.fireClose(code, reason);
  }
}

export function connectWebSocket(url: string): Socket {
  try {
    return new WsSocket(new WebSocket(url) as unknown as WebSocketLike);
  } catch (e) {
    // Invalid URL etc.: a socket that closes immediately.
    const s = new WsSocket({
      readyState: 3,
      binaryType: 'arraybuffer',
      onopen: null,
      onmessage: null,
      onclose: null,
      onerror: null,
      send() {
        throw e;
      },
      close() {},
    });
    queueMicrotask(() => s.close(1006, (e as Error).message));
    return s;
  }
}
