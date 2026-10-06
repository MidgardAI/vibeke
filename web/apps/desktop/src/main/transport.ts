// Sockets for core's channel, owned by the main process so connections outlive windows
// (spec 16 §16.1):
//   RelayTransport: `ws` WebSocket to a wss:// relay.
//   LocalTransport: `ws` WebSocket over the gateway's Unix socket `<gateway dir>/gateway.sock`.
// Both carry the same Noise channel; `parseConnectUrl` (core) picks one from the URL core builds.

import { connect as netConnect } from 'node:net';
import WebSocket from 'ws';
import { parseConnectUrl, type ConnectTarget, type Socket, type SocketState } from '@vibeke/core';

/** 16 MiB reassembly limit (spec 16 §5) plus framing headroom. */
const MAX_PAYLOAD = 17 * 1024 * 1024;
const HANDSHAKE_TIMEOUT_MS = 15_000;

/** The `ws` constructor arguments for a target (exported for tests). */
export function wsArgs(target: ConnectTarget): { url: string; options: WebSocket.ClientOptions } {
  const options: WebSocket.ClientOptions = {
    perMessageDeflate: false,
    maxPayload: MAX_PAYLOAD,
    handshakeTimeout: HANDSHAKE_TIMEOUT_MS,
    followRedirects: false,
  };
  if (target.kind === 'relay') return { url: target.url, options };
  // ws's `ws+unix:` URLs split on ':' and percent-encode spaces ("Application Support"), so
  // connect the socket ourselves and give ws a placeholder host.
  const socketPath = target.socketPath;
  return {
    url: `ws://localhost${target.path}`,
    options: { ...options, createConnection: () => netConnect({ path: socketPath }) } as WebSocket.ClientOptions,
  };
}

export class NodeSocket implements Socket {
  state: SocketState = 'connecting';
  onopen: (() => void) | null = null;
  onmessage: ((data: string | Uint8Array) => void) | null = null;
  onclose: ((code: number, reason: string) => void) | null = null;
  private fired = false;
  private ws: WebSocket | null = null;

  constructor(url: string) {
    let target: ConnectTarget;
    try {
      target = parseConnectUrl(url);
      const { url: u, options } = wsArgs(target);
      this.ws = new WebSocket(u, options);
    } catch (e) {
      queueMicrotask(() => this.fire(1006, (e as Error).message));
      return;
    }
    const ws = this.ws;
    ws.binaryType = 'nodebuffer';
    ws.on('open', () => {
      if (this.state !== 'connecting') return;
      this.state = 'open';
      this.onopen?.();
    });
    ws.on('message', (data, isBinary) => {
      if (this.state !== 'open') return;
      const buf = Array.isArray(data) ? Buffer.concat(data) : Buffer.isBuffer(data) ? data : Buffer.from(data as ArrayBuffer);
      if (isBinary) this.onmessage?.(new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength));
      else this.onmessage?.(buf.toString('utf8'));
    });
    ws.on('error', (err) => this.fire(1006, err.message));
    ws.on('close', (code, reason) => this.fire(code || 1006, reason.toString('utf8')));
  }

  private fire(code: number, reason: string): void {
    if (this.fired) return;
    this.fired = true;
    this.state = 'closed';
    this.onclose?.(code, reason);
  }

  send(data: string | Uint8Array): void {
    if (this.state !== 'open' || !this.ws) throw new Error(`send on ${this.state} socket`);
    // Preserve message type: strings are text frames, bytes are binary frames.
    if (typeof data === 'string') this.ws.send(data);
    else this.ws.send(data, { binary: true });
  }

  close(code = 1000, reason = ''): void {
    if (this.state === 'closed') return;
    try {
      if (this.ws && this.ws.readyState === WebSocket.CONNECTING) this.ws.terminate();
      else this.ws?.close(code >= 4000 || code === 1000 ? code : 1000, reason);
    } catch {
      this.ws?.terminate();
    }
    this.fire(code, reason);
  }
}

export const connectNode = (url: string): Socket => new NodeSocket(url);
