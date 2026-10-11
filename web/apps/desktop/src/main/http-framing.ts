// HTTP/1.x message framing for the preview proxy's keep-alive path: where a request or response
// body ends (Content-Length, chunked, or "until close"), so one upstream connection can carry one
// request after another. The proxy forwards the bytes unchanged; these helpers only find the
// boundaries. Electron-free for unit tests.

import type { Readable, Writable } from 'node:stream';

export type Framing = { kind: 'none' } | { kind: 'length'; n: number } | { kind: 'chunked' } | { kind: 'close' };

/** Pulls buffers from a stream on demand (paused in between) and can put bytes back. */
export class ByteReader {
  private queue: Buffer[] = [];
  private ended = false;
  private err: Error | null = null;
  private waiter: (() => void) | null = null;
  private readonly onData = (d: Buffer) => {
    this.queue.push(d);
    this.s.pause();
    this.wake();
  };
  private readonly onEnd = () => {
    this.ended = true;
    this.wake();
  };
  private readonly onError = (e: Error) => {
    this.err = e;
    this.wake();
  };

  constructor(private readonly s: Readable) {
    s.on('data', this.onData);
    s.on('end', this.onEnd);
    s.on('close', this.onEnd);
    s.on('error', this.onError);
    s.pause();
  }

  /** Bytes buffered and not read yet. */
  get buffered(): number {
    return this.queue.reduce((n, b) => n + b.length, 0);
  }

  get done(): boolean {
    return (this.ended || !!this.err) && !this.queue.length;
  }

  /** The next bytes, or null at the end of the stream. */
  async next(): Promise<Buffer | null> {
    for (;;) {
      const b = this.queue.shift();
      if (b) return b;
      if (this.err) throw this.err;
      if (this.ended) return null;
      const wait = new Promise<void>((r) => (this.waiter = r));
      this.s.resume();
      await wait;
    }
  }

  unread(b: Buffer): void {
    if (b.length) this.queue.unshift(b);
  }

  /** Stop reading: hand back what is buffered (the stream stays paused, for `pipe`). */
  detach(): Buffer {
    this.s.off('data', this.onData);
    this.s.off('end', this.onEnd);
    this.s.off('close', this.onEnd);
    this.s.off('error', this.onError);
    this.s.pause();
    const out = Buffer.concat(this.queue);
    this.queue = [];
    return out;
  }

  private wake(): void {
    const w = this.waiter;
    this.waiter = null;
    w?.();
  }
}

export type HeadResult = { head: Buffer } | { error: 'closed' | 'too_large' | 'timeout' };

/**
 * Reads up to and including the blank line that ends a message head. Returns the head without
 * the blank line; the bytes after it go back into the reader.
 */
export async function readHead(r: ByteReader, max: number, timeoutMs?: number): Promise<HeadResult> {
  let buf: Buffer<ArrayBufferLike> = Buffer.alloc(0);
  let timer: ReturnType<typeof setTimeout> | null = null;
  const timeout = timeoutMs
    ? new Promise<'timeout'>((res) => {
        timer = setTimeout(() => res('timeout'), timeoutMs);
        (timer as { unref?: () => void }).unref?.();
      })
    : null;
  try {
    for (;;) {
      const chunk = timeout ? await Promise.race([r.next(), timeout]) : await r.next();
      if (chunk === 'timeout') return { error: 'timeout' };
      if (chunk === null) return { error: 'closed' };
      buf = buf.length ? Buffer.concat([buf, chunk]) : chunk;
      const end = buf.indexOf('\r\n\r\n');
      if (end >= 0) {
        if (end > max) return { error: 'too_large' };
        r.unread(buf.subarray(end + 4));
        return { head: buf.subarray(0, end) };
      }
      if (buf.length > max) return { error: 'too_large' };
    }
  } finally {
    if (timer) clearTimeout(timer);
  }
}

/** Tracks a chunked body as raw bytes pass by; `feed` says how many belong to the message. */
export class ChunkedTracker {
  private state: 'size' | 'data' | 'trailer' = 'size';
  private line = '';
  private left = 0;

  feed(b: Buffer): { used: number; done: boolean } {
    let i = 0;
    while (i < b.length) {
      if (this.state === 'data') {
        const take = Math.min(this.left, b.length - i);
        i += take;
        this.left -= take;
        if (this.left === 0) this.state = 'size';
        continue;
      }
      const c = b[i++]!;
      if (c !== 0x0a) {
        if (this.line.length > 8192) throw new Error('chunk line too long');
        this.line += String.fromCharCode(c);
        continue;
      }
      const line = this.line.replace(/\r$/, '');
      this.line = '';
      if (this.state === 'trailer') {
        if (line === '') return { used: i, done: true };
        continue;
      }
      const m = /^([0-9a-fA-F]{1,16})\s*(;.*)?$/.exec(line.trim());
      if (!m) throw new Error('bad chunk size');
      const n = parseInt(m[1]!, 16);
      if (n === 0) this.state = 'trailer';
      else {
        this.state = 'data';
        this.left = n + 2; // the data and its CRLF
      }
    }
    return { used: i, done: false };
  }
}

const headerOf = (headers: [string, string][], name: string): string | undefined => headers.find(([k]) => k.toLowerCase() === name)?.[1];

/** Body framing of a request (RFC 9112 §6.3): chunked, Content-Length, else none. Null when invalid. */
export function requestFraming(headers: [string, string][]): Framing | null {
  const te = headerOf(headers, 'transfer-encoding');
  if (te) return /(^|,)\s*chunked\s*$/i.test(te) ? { kind: 'chunked' } : null;
  const cl = headerOf(headers, 'content-length');
  if (cl === undefined) return { kind: 'none' };
  if (!/^\d{1,15}$/.test(cl.trim())) return null;
  const n = Number(cl.trim());
  return n === 0 ? { kind: 'none' } : { kind: 'length', n };
}

export interface ResponseHead {
  version: '1.0' | '1.1';
  status: number;
  headers: [string, string][];
}

export function parseResponseHead(text: string): ResponseHead | null {
  const lines = text.split('\r\n');
  const m = /^HTTP\/1\.([01]) (\d{3})(?: .*)?$/.exec(lines[0] ?? '');
  if (!m) return null;
  const headers: [string, string][] = [];
  for (const line of lines.slice(1)) {
    const i = line.indexOf(':');
    if (i <= 0) return null;
    headers.push([line.slice(0, i).trim(), line.slice(i + 1).trim()]);
  }
  return { version: m[1] === '0' ? '1.0' : '1.1', status: Number(m[2]), headers };
}

/** Body framing of a response to `method` (RFC 9112 §6.3). Null when invalid. */
export function responseFraming(method: string, r: ResponseHead): Framing | null {
  if (method === 'HEAD' || (r.status >= 100 && r.status < 200) || r.status === 204 || r.status === 304) return { kind: 'none' };
  const te = headerOf(r.headers, 'transfer-encoding');
  if (te) return /(^|,)\s*chunked\s*$/i.test(te) ? { kind: 'chunked' } : { kind: 'close' };
  const cl = headerOf(r.headers, 'content-length');
  if (cl === undefined) return { kind: 'close' };
  if (!/^\d{1,15}$/.test(cl.trim())) return null;
  const n = Number(cl.trim());
  return n === 0 ? { kind: 'none' } : { kind: 'length', n };
}

/** May the connection carry another message after this one (per its own headers)? */
export function keepAlive(version: '1.0' | '1.1', headers: [string, string][]): boolean {
  const tokens = (headerOf(headers, 'connection') ?? '')
    .split(',')
    .map((s) => s.trim().toLowerCase());
  if (tokens.includes('close')) return false;
  return version === '1.1' || tokens.includes('keep-alive');
}

/** Write with backpressure: resolves once the stream can take more. */
export function writeAll(s: Writable, b: Buffer): Promise<void> {
  if (!b.length) return Promise.resolve();
  return new Promise((resolve, reject) => {
    if (s.destroyed || s.writableEnded) return reject(new Error('stream closed'));
    if (s.write(b)) return resolve();
    const done = () => {
      off();
      resolve();
    };
    const fail = () => {
      off();
      reject(new Error('stream closed'));
    };
    const off = () => {
      s.off('drain', done);
      s.off('close', fail);
      s.off('error', fail);
    };
    s.once('drain', done);
    s.once('close', fail);
    s.once('error', fail);
  });
}

/** Copy one body from `r` to `out`; bytes after it go back into `r`. Returns the bytes copied. */
export async function copyBody(r: ByteReader, f: Framing, out: Writable): Promise<number> {
  if (f.kind === 'none') return 0;
  let copied = 0;
  if (f.kind === 'length') {
    let left = f.n;
    while (left > 0) {
      const b = await r.next();
      if (!b) throw new Error('body ended early');
      const take = Math.min(left, b.length);
      await writeAll(out, b.subarray(0, take));
      r.unread(b.subarray(take));
      left -= take;
      copied += take;
    }
    return copied;
  }
  if (f.kind === 'chunked') {
    const t = new ChunkedTracker();
    for (;;) {
      const b = await r.next();
      if (!b) throw new Error('body ended early');
      const { used, done } = t.feed(b);
      await writeAll(out, b.subarray(0, used));
      copied += used;
      if (done) {
        r.unread(b.subarray(used));
        return copied;
      }
    }
  }
  for (;;) {
    const b = await r.next();
    if (!b) return copied;
    await writeAll(out, b);
    copied += b.length;
  }
}
