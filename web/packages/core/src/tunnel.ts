// The device side of the gateway's stream tunnel without Node streams (spec 06 B3.5, spec 20
// §3.6): the browser's review controller opens one stream per request or WebSocket. Callbacks
// instead of a Duplex; the same wire rules as the desktop's `preview-tunnel.ts`:
//
// - `data_b64` is unpadded base64url; a `tunnel.send` carries at most 64 KiB and the next one
//   waits for its response (backpressure).
// - Received data is acknowledged (`tunnel.ack`) in 64 KiB batches once handed to the reader.
// - `tunnel.closed` with `eof` ends the host→device direction; any other reason ends the stream.

import * as b64 from './b64';
import { TUNNEL_SEND_MAX, type TunnelOpenParams, type TunnelOpenPreviewParams } from './model';
import type { RpcLink } from './rpc';

const ACK_BATCH = 64 * 1024;
const OPEN_TIMEOUT_MS = 20_000;
const SEND_TIMEOUT_MS = 60_000;
const MAX_DATA_B64 = 96 * 1024;
const MAX_EARLY_BYTES = 1024 * 1024;

export class TunnelClosedError extends Error {
  constructor(readonly reason: string) {
    super(`tunnel stream ${reason}`);
    this.name = 'TunnelClosedError';
  }
}

export interface TunnelHandlers {
  data(bytes: Uint8Array): void;
  /** The host→device direction ended (`eof`); writes may continue. */
  end(): void;
  /** The stream is over (`closed`, `error`, `revoked`, `disconnected`, or `local`). */
  close(reason: string): void;
}

export class TunnelConn {
  private pendingAck = 0;
  private gone = false;
  private sending: Promise<void> = Promise.resolve();
  private h: TunnelHandlers | null = null;
  private early: ({ data: Uint8Array } | { reason: string })[] = [];

  constructor(
    private readonly link: RpcLink,
    readonly id: string,
    private readonly onDone: (c: TunnelConn) => void,
  ) {}

  get closed(): boolean {
    return this.gone;
  }

  /** Start delivering; events that came before are delivered now, in order. */
  listen(h: TunnelHandlers): void {
    this.h = h;
    const q = this.early;
    this.early = [];
    for (const e of q) {
      if ('data' in e) this.receive(e.data);
      else this.remoteClosed(e.reason);
    }
  }

  receive(data: Uint8Array): void {
    if (this.gone || !data.length) return;
    if (!this.h) {
      this.early.push({ data });
      return;
    }
    this.h.data(data);
    this.pendingAck += data.length;
    if (this.pendingAck >= ACK_BATCH) this.flushAck();
  }

  remoteClosed(reason: string): void {
    if (this.gone) return;
    if (!this.h) {
      this.early.push({ reason });
      return;
    }
    if (reason === 'eof') {
      this.flushAck();
      this.h.end();
      return;
    }
    this.finish(reason, false);
  }

  private flushAck(): void {
    const bytes = this.pendingAck;
    this.pendingAck = 0;
    if (!bytes || this.gone || this.link.closed) return;
    this.link.request('tunnel.ack', { stream: this.id, bytes }).catch(() => {});
  }

  /** Write bytes; resolves when the gateway accepted the last piece. Writes are serialized. */
  write(bytes: Uint8Array): Promise<void> {
    const run = async () => {
      for (let at = 0; at < bytes.length; at += TUNNEL_SEND_MAX) {
        if (this.gone || this.link.closed) throw new TunnelClosedError('closed');
        const piece = bytes.subarray(at, at + TUNNEL_SEND_MAX);
        await this.link.request('tunnel.send', { stream: this.id, data_b64: b64.encode(piece) }, { timeoutMs: SEND_TIMEOUT_MS });
      }
    };
    const p = this.sending.then(run);
    this.sending = p.catch(() => {});
    return p;
  }

  /** Half-close: no more writes from this side. */
  end(): Promise<void> {
    const p = this.sending.then(async () => {
      if (this.gone || this.link.closed) return;
      await this.link.request('tunnel.send', { stream: this.id, eof: true }, { timeoutMs: SEND_TIMEOUT_MS }).catch(() => {});
    });
    this.sending = p.catch(() => {});
    return p;
  }

  /** Close the stream from this side. */
  close(): void {
    this.finish('local', true);
  }

  /** The host connection ended. */
  linkClosed(): void {
    this.finish('disconnected', false);
  }

  private finish(reason: string, tellHost: boolean): void {
    if (this.gone) return;
    this.gone = true;
    if (tellHost && !this.link.closed) this.link.request('tunnel.close', { stream: this.id }).catch(() => {});
    this.onDone(this);
    this.h?.close(reason);
  }
}

/** Every stream of one connection: routes `tunnel.data` / `tunnel.closed` by stream id. */
export class TunnelClient {
  private readonly streams = new Map<string, TunnelConn>();
  private early = new Map<string, ({ data: Uint8Array } | { reason: string })[]>();
  private earlyBytes = 0;
  private opening = 0;
  private readonly offs: (() => void)[];
  private dead = false;

  constructor(private readonly link: RpcLink) {
    this.offs = [
      link.on('tunnel.data', (p) => this.onData(p)),
      link.on('tunnel.closed', (p) => this.onClosed(p)),
      link.onClose(() => this.onLinkClosed()),
    ];
  }

  get size(): number {
    return this.streams.size;
  }

  get closed(): boolean {
    return this.dead || this.link.closed;
  }

  async open(params: TunnelOpenParams | TunnelOpenPreviewParams): Promise<TunnelConn> {
    if (this.closed) throw new TunnelClosedError('disconnected');
    this.opening++;
    try {
      const r = await this.link.request<{ stream?: unknown }>('tunnel.open', { ...params }, { timeoutMs: OPEN_TIMEOUT_MS });
      const id = r?.stream;
      if (typeof id !== 'string' || !id || id.length > 200) throw new TunnelClosedError('error');
      const c = new TunnelConn(this.link, id, (x) => {
        if (this.streams.get(id) === x) this.streams.delete(id);
      });
      if (this.closed) {
        c.linkClosed();
        throw new TunnelClosedError('disconnected');
      }
      this.streams.set(id, c);
      const queued = this.early.get(id);
      if (queued) {
        this.early.delete(id);
        for (const q of queued) {
          if ('data' in q) {
            this.earlyBytes -= q.data.length;
            c.receive(q.data);
          } else c.remoteClosed(q.reason);
        }
      }
      return c;
    } finally {
      if (--this.opening === 0) {
        this.early.clear();
        this.earlyBytes = 0;
      }
    }
  }

  /** Stop routing (the controller went away); open streams are closed. */
  dispose(): void {
    if (this.dead) return;
    for (const s of [...this.streams.values()]) s.close();
    this.onLinkClosed();
  }

  private onData(p: unknown): void {
    const v = p as { stream?: unknown; data_b64?: unknown };
    if (typeof v?.stream !== 'string' || typeof v.data_b64 !== 'string') return;
    const s = this.streams.get(v.stream);
    if (v.data_b64.length > MAX_DATA_B64) {
      s?.close();
      return;
    }
    let data: Uint8Array;
    try {
      data = b64.decode(v.data_b64);
    } catch {
      s?.close();
      return;
    }
    if (s) return s.receive(data);
    this.queue(v.stream, { data });
  }

  private onClosed(p: unknown): void {
    const v = p as { stream?: unknown; reason?: unknown };
    if (typeof v?.stream !== 'string') return;
    const reason = typeof v.reason === 'string' ? v.reason : 'closed';
    const s = this.streams.get(v.stream);
    if (s) return s.remoteClosed(reason);
    this.queue(v.stream, { reason });
  }

  private queue(id: string, e: { data: Uint8Array } | { reason: string }): void {
    if (!this.opening) return;
    const n = 'data' in e ? e.data.length : 0;
    if (this.earlyBytes + n > MAX_EARLY_BYTES) return;
    this.earlyBytes += n;
    const list = this.early.get(id);
    if (list) list.push(e);
    else this.early.set(id, [e]);
  }

  private onLinkClosed(): void {
    if (this.dead) return;
    this.dead = true;
    for (const off of this.offs) off();
    for (const s of [...this.streams.values()]) s.linkClosed();
    this.streams.clear();
    this.early.clear();
  }
}
