// Test doubles: a deterministic clock, an in-memory socket pair and a tiny mock gateway that
// speaks the real channel (TS Responder) so Channel/RpcClient/pair()/HostManager run end to end.

import { randomBytes } from '@noble/hashes/utils.js';
import { parseHello } from '../src/hello';
import { Responder, type Session } from '../src/noise';
import type { Clock, KeyStore, Lifecycle, Platform, Socket, SocketState } from '../src/platform';

/** Let pending promise continuations and queued socket deliveries run. */
export async function flush(rounds = 5): Promise<void> {
  for (let i = 0; i < rounds; i++) await new Promise<void>((r) => setImmediate(r));
}

export class FakeClock implements Clock {
  private t: number;
  private seq = 0;
  private timers = new Map<number, { at: number; fn: () => void }>();
  constructor(start = 1_800_000_000_000) {
    this.t = start;
  }
  now(): number {
    return this.t;
  }
  setTimeout(fn: () => void, ms: number): number {
    const id = ++this.seq;
    this.timers.set(id, { at: this.t + Math.max(0, ms), fn });
    return id;
  }
  clearTimeout(h: unknown): void {
    this.timers.delete(h as number);
  }
  get pending(): number {
    return this.timers.size;
  }
  /** Advance time, firing due timers in order and flushing async work between them. */
  async advance(ms: number): Promise<void> {
    const target = this.t + ms;
    for (;;) {
      await flush();
      let next: [number, { at: number; fn: () => void }] | undefined;
      for (const e of this.timers) if (e[1].at <= target && (!next || e[1].at < next[1].at)) next = e;
      if (!next) break;
      this.timers.delete(next[0]);
      this.t = next[1].at;
      next[1].fn();
    }
    this.t = target;
    await flush();
  }
}

class MemSocket implements Socket {
  state: SocketState = 'connecting';
  peer!: MemSocket;
  onopen: (() => void) | null = null;
  onmessage: ((data: string | Uint8Array) => void) | null = null;
  onclose: ((code: number, reason: string) => void) | null = null;
  /** Every message this end sent (copies). */
  sent: (string | Uint8Array)[] = [];
  closedWith: { code: number; reason: string } | null = null;

  send(data: string | Uint8Array): void {
    if (this.state !== 'open') throw new Error(`send on ${this.state} socket`);
    const copy = typeof data === 'string' ? data : new Uint8Array(data);
    this.sent.push(copy);
    queueMicrotask(() => {
      if (this.peer.state === 'open') this.peer.onmessage?.(copy);
    });
  }
  close(code = 1000, reason = ''): void {
    if (this.state === 'closed') return;
    this.state = 'closed';
    this.closedWith = { code, reason };
    // Like a WebSocket: messages already sent are delivered before the peer sees the close.
    queueMicrotask(() => {
      this.onclose?.(code, reason);
      if (this.peer.state !== 'closed') {
        this.peer.state = 'closed';
        this.peer.closedWith = { code, reason };
        this.peer.onclose?.(code, reason);
      }
    });
  }
}

/** Two connected sockets; both open on the next microtask. */
export function socketPair(): [MemSocket, MemSocket] {
  const a = new MemSocket();
  const b = new MemSocket();
  a.peer = b;
  b.peer = a;
  queueMicrotask(() => {
    for (const s of [a, b]) {
      if (s.state === 'connecting') {
        s.state = 'open';
        s.onopen?.();
      }
    }
  });
  return [a, b];
}

export type { MemSocket };

export interface GatewayCtx {
  deviceKey: Uint8Array;
  hello: string;
  send(obj: unknown): void;
  notify(method: string, params?: unknown): void;
  close(code?: number): void;
  socket: MemSocket;
  session: Session;
}

export interface MockGatewayOptions {
  hostPrivate: Uint8Array;
  /** Pairing secrets by pid. */
  psks?: Record<string, Uint8Array>;
  /** Return false to send plaintext {"error":"unauthorized"} and close after message 1. */
  authorize?(deviceKey: Uint8Array): boolean;
  hostInfo?: Record<string, unknown>;
  /** JSON-RPC handler. Throw {code,message,data} to answer with an error; return undefined to not answer. */
  handle?(method: string, params: any, ctx: GatewayCtx): unknown;
  onReady?(ctx: GatewayCtx): void;
}

/** Serve the gateway side of one connection on `sock`. */
export function serveGateway(sock: MemSocket, o: MockGatewayOptions): void {
  let prologue: string | null = null;
  let responder: Responder | null = null;
  let ctx: GatewayCtx | null = null;
  sock.onmessage = (data) => {
    try {
      if (prologue === null) {
        if (typeof data !== 'string') return sock.close(4002);
        prologue = data;
        const h = parseHello(data);
        const psk = h.mode === 'pair' ? o.psks?.[h.pid!] : undefined;
        if (h.mode === 'pair' && !psk) {
          sock.send('{"error":"unauthorized"}');
          return sock.close(4401);
        }
        responder = new Responder({ prologue: new TextEncoder().encode(data), localPrivate: o.hostPrivate, psk });
        return;
      }
      if (typeof data === 'string') return sock.close(4002);
      if (!ctx) {
        const { remoteStatic } = responder!.readFirst(data);
        if (o.authorize && !o.authorize(remoteStatic)) {
          sock.send('{"error":"unauthorized"}');
          return sock.close(4401);
        }
        const info = o.hostInfo ?? { v: 1, host_name: 'devbox', gateway_version: '0.1.0', server_version: '0.1.0' };
        const { message, session } = responder!.writeSecond(new TextEncoder().encode(JSON.stringify(info)));
        sock.send(message);
        const send = (obj: unknown) => {
          for (const f of session.encrypt(new TextEncoder().encode(JSON.stringify(obj)))) sock.send(f);
        };
        ctx = {
          deviceKey: remoteStatic,
          hello: prologue,
          send,
          notify: (method, params = {}) => send({ jsonrpc: '2.0', method, params }),
          close: (code = 1000) => sock.close(code),
          socket: sock,
          session,
        };
        o.onReady?.(ctx);
        return;
      }
      const plain = ctx.session.decrypt(data);
      if (!plain) return;
      const msg = JSON.parse(new TextDecoder().decode(plain));
      if (msg.id === undefined) return;
      const c = ctx;
      Promise.resolve()
        .then(() => o.handle?.(msg.method, msg.params, c))
        .then(
          (result) => {
            if (result !== undefined && sock.state === 'open') c.send({ jsonrpc: '2.0', id: msg.id, result });
          },
          (err) => {
            if (sock.state === 'open') c.send({ jsonrpc: '2.0', id: msg.id, error: err });
          },
        );
    } catch {
      sock.close(4002);
    }
  };
}

export class MemKeyStore implements KeyStore {
  m = new Map<string, Uint8Array>();
  async get(n: string) {
    return this.m.get(n) ?? null;
  }
  async set(n: string, v: Uint8Array) {
    this.m.set(n, v);
  }
  async delete(n: string) {
    this.m.delete(n);
  }
}

export class FakeLifecycle implements Lifecycle {
  visible = true;
  private v = new Set<() => void>();
  private h = new Set<() => void>();
  onVisible(cb: () => void) {
    this.v.add(cb);
    return () => this.v.delete(cb);
  }
  onHidden(cb: () => void) {
    this.h.add(cb);
    return () => this.h.delete(cb);
  }
  isVisible() {
    return this.visible;
  }
  show() {
    this.visible = true;
    this.v.forEach((f) => f());
  }
  hide() {
    this.visible = false;
    this.h.forEach((f) => f());
  }
}

export interface TestPlatform extends Platform {
  clock: FakeClock;
  lifecycle: FakeLifecycle;
  urls: string[];
}

/** A Platform whose connect() hands the server end to `gateway` (or refuses when it returns false). */
export function testPlatform(gateway: (sock: MemSocket, url: string) => void | false): TestPlatform {
  const urls: string[] = [];
  return {
    keystore: new MemKeyStore(),
    clock: new FakeClock(),
    random: (n) => randomBytes(n),
    lifecycle: new FakeLifecycle(),
    platformName: 'test',
    urls,
    connect(url) {
      urls.push(url);
      const [client, server] = socketPair();
      if (gateway(server, url) === false) queueMicrotask(() => server.close(4404, 'host_offline'));
      return client;
    },
  };
}
