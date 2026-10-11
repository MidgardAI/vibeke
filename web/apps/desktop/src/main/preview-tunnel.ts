// The device side of the gateway's stream tunnel (spec 06 B3.5, `tunnel.*`): a Node duplex per
// stream over one host connection. Electron-free so the proxy tests can use it with a fake link.
//
// Backpressure both ways:
// - writes: each `tunnel.send` (≤ 64 KiB) waits for its response before the next one, and the
//   write callback runs only after the last piece was answered;
// - reads: data is acknowledged (`tunnel.ack`) once the readable buffer has room for it, so the
//   gateway stops reading from the dev server while a slow page has 512 KiB unacknowledged.
//
// Wire: `data_b64` is unpadded base64url (vk_e2e::b64; Node's 'base64url'). `tunnel.closed`
// with `eof` ends only the host→device direction; a later `closed`/`error`/`revoked` ends the
// stream. `tunnel.close` gets no notification back. One `tunnel.send` is in flight per stream
// (Node serializes `_write`), so the gateway's 8-write queue never answers `busy`.

import { Duplex } from 'node:stream';
import { TUNNEL_ACK_WINDOW, TUNNEL_SEND_MAX, type RpcLink, type TunnelOpenParams } from '@vibeke/core';

/** Acknowledge in batches of this many bytes (the gateway's window is 512 KiB). */
const ACK_BATCH = 64 * 1024;
/** Bytes the readable side buffers before it stops acknowledging. */
const READ_BUFFER = 256 * 1024;
const OPEN_TIMEOUT_MS = 20_000;
const SEND_TIMEOUT_MS = 60_000;
/** A `tunnel.data` chunk is ≤ 48 KiB (64 Ki base64 characters); anything far larger is refused. */
const MAX_DATA_B64 = 96 * 1024;
/**
 * Data for a stream whose `tunnel.open` response has not arrived yet. The gateway sends at most
 * `TUNNEL_ACK_WINDOW` unacknowledged bytes per stream, so this holds a full window with room to
 * spare; a stream that still goes over fails with an error instead of losing bytes.
 */
const MAX_EARLY_STREAM_BYTES = TUNNEL_ACK_WINDOW + 128 * 1024;
/** All early data together (several streams opening at once). */
const MAX_EARLY_BYTES = 8 * 1024 * 1024;

export class TunnelError extends Error {
  constructor(
    message: string,
    readonly reason: string,
  ) {
    super(message);
    this.name = 'TunnelError';
  }
}

export class TunnelStream extends Duplex {
  /** Delivered to the readable buffer, not yet acknowledged. */
  private pendingAck = 0;
  /** Pushed while the readable buffer was over its mark: acknowledged on the next `_read`. */
  private heldAck = 0;
  /** The gateway already forgot this stream (`tunnel.closed` other than eof). */
  private gone = false;

  constructor(
    private readonly link: RpcLink,
    readonly id: string,
    private readonly onDone: (s: TunnelStream) => void,
  ) {
    super({ readableHighWaterMark: READ_BUFFER, writableHighWaterMark: TUNNEL_SEND_MAX, allowHalfOpen: true });
  }

  /** `tunnel.data` for this stream (from the mux). */
  receive(data: Buffer): void {
    if (this.destroyed || this.readableEnded || !data.length) return;
    if (this.push(data)) this.consumed(data.length);
    else this.heldAck += data.length;
  }

  /** `tunnel.closed` for this stream. */
  remoteClosed(reason: string): void {
    if (this.destroyed) return;
    if (reason === 'eof') {
      this.push(null);
      return;
    }
    // `closed` may follow an earlier `eof`: the stream is over, but data already buffered for the
    // reader is still delivered before the stream is destroyed.
    this.gone = true;
    if (reason === 'closed') {
      this.push(null);
      if (this.readableEnded) this.destroy();
      else this.once('end', () => this.destroy());
    } else {
      this.destroy(new TunnelError(`tunnel stream ${reason}`, reason));
    }
  }

  /** The host connection ended: the stream cannot continue. */
  linkClosed(): void {
    this.gone = true;
    this.destroy(new TunnelError('host disconnected', 'disconnected'));
  }

  override _read(): void {
    if (!this.heldAck) return;
    const n = this.heldAck;
    this.heldAck = 0;
    this.consumed(n);
  }

  private consumed(n: number): void {
    this.pendingAck += n;
    if (this.pendingAck >= ACK_BATCH) this.flushAck();
  }

  private flushAck(): void {
    const bytes = this.pendingAck;
    this.pendingAck = 0;
    if (!bytes || this.gone || this.link.closed) return;
    this.link.request('tunnel.ack', { stream: this.id, bytes }).catch(() => {});
  }

  override _write(chunk: Buffer, _enc: BufferEncoding, cb: (e?: Error | null) => void): void {
    void this.sendAll(chunk).then(
      () => cb(),
      (e: unknown) => cb(e instanceof Error ? e : new Error(String(e))),
    );
  }

  private async sendAll(chunk: Buffer): Promise<void> {
    for (let at = 0; at < chunk.length; at += TUNNEL_SEND_MAX) {
      if (this.gone || this.link.closed) throw new TunnelError('tunnel stream closed', 'closed');
      const piece = chunk.subarray(at, at + TUNNEL_SEND_MAX);
      await this.link.request('tunnel.send', { stream: this.id, data_b64: piece.toString('base64url') }, { timeoutMs: SEND_TIMEOUT_MS });
    }
  }

  override _final(cb: (e?: Error | null) => void): void {
    if (this.gone || this.link.closed) return cb();
    this.link.request('tunnel.send', { stream: this.id, eof: true }, { timeoutMs: SEND_TIMEOUT_MS }).then(
      () => cb(),
      () => cb(),
    );
  }

  override _destroy(err: Error | null, cb: (e?: Error | null) => void): void {
    if (!this.gone && !this.link.closed) this.link.request('tunnel.close', { stream: this.id }).catch(() => {});
    this.gone = true;
    this.onDone(this);
    cb(err);
  }
}

type Early = { data?: Buffer; reason?: string };

/** All tunnel streams of one host connection: routes `tunnel.data` / `tunnel.closed` by stream id. */
export class TunnelMux {
  private readonly streams = new Map<string, TunnelStream>();
  /** Notifications that arrived for an unknown stream while an open was in flight. */
  private early = new Map<string, Early[]>();
  private earlyBytes = 0;
  /** Streams whose early data did not fit: they open as failed, never with a gap in the data. */
  private overflowed = new Set<string>();
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

  async open(params: TunnelOpenParams): Promise<TunnelStream> {
    if (this.dead || this.link.closed) throw new TunnelError('host disconnected', 'disconnected');
    this.opening++;
    try {
      const r = await this.link.request<{ stream?: unknown }>('tunnel.open', { ...params }, { timeoutMs: OPEN_TIMEOUT_MS });
      const id = r?.stream;
      if (typeof id !== 'string' || !id || id.length > 200) throw new TunnelError('invalid tunnel.open response', 'error');
      const s = new TunnelStream(this.link, id, (x) => {
        if (this.streams.get(id) === x) this.streams.delete(id);
      });
      if (this.dead || this.link.closed) {
        s.linkClosed();
        throw new TunnelError('host disconnected', 'disconnected');
      }
      this.streams.set(id, s);
      if (this.overflowed.delete(id)) {
        this.early.delete(id);
        s.destroy(new TunnelError('the preview sent data faster than the app could take it; reload the page', 'overflow'));
        throw new TunnelError('tunnel stream overflowed before it opened', 'overflow');
      }
      const queued = this.early.get(id);
      if (queued) {
        this.early.delete(id);
        for (const q of queued) {
          if (q.data) this.earlyBytes -= q.data.length;
          if (q.data) s.receive(q.data);
          else if (q.reason !== undefined) s.remoteClosed(q.reason);
        }
      }
      return s;
    } finally {
      if (--this.opening === 0) {
        this.early.clear();
        this.earlyBytes = 0;
        this.overflowed.clear();
      }
    }
  }

  private onData(p: unknown): void {
    const v = p as { stream?: unknown; data_b64?: unknown };
    if (typeof v?.stream !== 'string' || typeof v.data_b64 !== 'string') return;
    if (v.data_b64.length > MAX_DATA_B64) {
      this.streams.get(v.stream)?.destroy(new TunnelError('tunnel chunk too large', 'error'));
      return;
    }
    const data = Buffer.from(v.data_b64, 'base64url');
    const s = this.streams.get(v.stream);
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

  private queue(id: string, e: Early): void {
    if (!this.opening || this.overflowed.has(id)) return;
    const n = e.data?.length ?? 0;
    const list = this.early.get(id);
    const streamBytes = (list ?? []).reduce((sum, q) => sum + (q.data?.length ?? 0), 0);
    if (this.earlyBytes + n > MAX_EARLY_BYTES || streamBytes + n > MAX_EARLY_STREAM_BYTES) {
      // Dropping bytes would corrupt the stream silently: fail it loudly when it opens.
      this.overflowed.add(id);
      if (list) {
        this.earlyBytes -= streamBytes;
        this.early.delete(id);
      }
      this.link.request('tunnel.close', { stream: id }).catch(() => {});
      return;
    }
    this.earlyBytes += n;
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

const muxes = new WeakMap<RpcLink, TunnelMux>();

/** The mux of a host connection (one per connection; a reconnect gets a new one). */
export function tunnelMux(link: RpcLink): TunnelMux {
  let m = muxes.get(link);
  if (!m) muxes.set(link, (m = new TunnelMux(link)));
  return m;
}

/** `tunnel.open` refused the port: `{kind: forbidden|permission_denied, details: {reason: port_not_allowed, port}}`. */
export function portNotAllowed(e: unknown): { port: number; host: string } | null {
  const x = e as { message?: unknown; data?: { details?: { reason?: unknown; port?: unknown; host?: unknown } } } | null;
  const d = x?.data?.details;
  const byDetails = d?.reason === 'port_not_allowed';
  const byMessage = typeof x?.message === 'string' && x.message.includes('port_not_allowed');
  if (!byDetails && !byMessage) return null;
  const port = typeof d?.port === 'number' ? d.port : Number(/port_not_allowed\D*(\d{1,5})/.exec(String(x?.message))?.[1] ?? NaN);
  if (!Number.isInteger(port) || port < 1 || port > 65535) return null;
  return { port, host: typeof d?.host === 'string' ? d.host : 'localhost' };
}
