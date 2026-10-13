// HTTP/1.1 over a tunnel stream (spec 20 §3.6): the review controller writes requests to the
// preview's dev server and parses its responses incrementally. One request per stream
// (`Connection: close`), so the parser handles exactly one response, plus 1xx interim responses
// and a `101 Switching Protocols` upgrade (WebSockets).
//
// No DOM: bytes in, events out.

export type HeaderList = [string, string][];

const enc = new TextEncoder();
const latin1 = new TextDecoder('latin1');

/** Request methods the controller forwards. */
export const HTTP_METHODS: ReadonlySet<string> = new Set(['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS']);

/** Hop-by-hop and connection-specific headers that never cross the tunnel. */
const HOP: ReadonlySet<string> = new Set([
  'connection',
  'keep-alive',
  'proxy-authenticate',
  'proxy-authorization',
  'proxy-connection',
  'te',
  'trailer',
  'transfer-encoding',
  'upgrade',
  'host',
  'content-length',
  'cookie',
]);

const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;
const BAD_VALUE = /[\r\n\0]/;

export interface RequestHead {
  method: string;
  /** Origin-form target: `/path?query`. */
  target: string;
  headers: HeaderList;
}

/** Serialize a request head and body. Throws on a malformed method, target or header. */
export function writeRequest(head: RequestHead, body?: Uint8Array | null): Uint8Array {
  if (!TOKEN.test(head.method)) throw new Error('http: bad method');
  if (!head.target.startsWith('/') || /[\s\0]/.test(head.target)) throw new Error('http: bad target');
  let text = `${head.method} ${head.target} HTTP/1.1\r\n`;
  for (const [k, v] of head.headers) {
    if (!TOKEN.test(k) || BAD_VALUE.test(v)) throw new Error(`http: bad header ${k}`);
    text += `${k}: ${v}\r\n`;
  }
  const hasLength = head.headers.some(([k]) => k.toLowerCase() === 'content-length');
  if (body && body.length && !hasLength) text += `Content-Length: ${body.length}\r\n`;
  else if (!hasLength && !body && (head.method === 'POST' || head.method === 'PUT' || head.method === 'PATCH')) text += 'Content-Length: 0\r\n';
  text += '\r\n';
  const h = enc.encode(text);
  if (!body || !body.length) return h;
  const out = new Uint8Array(h.length + body.length);
  out.set(h, 0);
  out.set(body, h.length);
  return out;
}

export interface ForwardOptions {
  /** The dev server's authority, e.g. `localhost:5173`. */
  target: string;
  /** The content origin the browser sees, e.g. `https://abcd….p.vibeke.dev`. */
  contentOrigin: string;
  /** The `Cookie` header from the controller's jar, or null. */
  cookie: string | null;
  /** Keep `Connection: Upgrade` / `Upgrade` (WebSocket handshakes). */
  upgrade?: boolean;
}

/**
 * Headers for the dev server: hop-by-hop headers and the browser's own `Cookie` dropped, `Host`
 * set to the dev server, `Origin` and `Referer` moved from the content origin to the dev server's,
 * the jar's cookies added, and `Accept-Encoding: identity` so bodies arrive uncompressed.
 */
export function forwardRequestHeaders(headers: HeaderList, o: ForwardOptions): HeaderList {
  const local = `http://${o.target}`;
  const out: HeaderList = [['Host', o.target]];
  for (const [k, v] of headers) {
    const l = k.toLowerCase();
    if (HOP.has(l) || l === 'accept-encoding' || l.startsWith('sec-websocket') || l.startsWith('proxy-')) continue;
    if (!TOKEN.test(k) || BAD_VALUE.test(v)) continue;
    if (l === 'origin') {
      out.push([k, v === o.contentOrigin ? local : v]);
    } else if (l === 'referer') {
      out.push([k, v.startsWith(`${o.contentOrigin}/`) || v === o.contentOrigin ? local + v.slice(o.contentOrigin.length) : v]);
    } else {
      out.push([k, v]);
    }
  }
  out.push(['Accept-Encoding', 'identity']);
  if (o.cookie) out.push(['Cookie', o.cookie]);
  if (o.upgrade) {
    out.push(['Connection', 'Upgrade']);
    out.push(['Upgrade', 'websocket']);
  } else {
    out.push(['Connection', 'close']);
  }
  return out;
}

/** Response headers the content origin never sees (the controller handles them). */
const RESPONSE_DROP: ReadonlySet<string> = new Set([
  'connection',
  'keep-alive',
  'transfer-encoding',
  'trailer',
  'upgrade',
  'proxy-connection',
  'set-cookie',
  'set-cookie2',
  'content-length',
  'x-frame-options',
]);

/**
 * Response headers for the content origin: `Set-Cookie` (the jar keeps it) and hop-by-hop headers
 * removed, `Location` moved from the dev server's origin to the content origin, and
 * `frame-ancestors` dropped from a CSP (the preview runs in the review screen's frame).
 */
export function forwardResponseHeaders(headers: HeaderList, o: { target: string; contentOrigin: string; decoded?: boolean }): HeaderList {
  const out: HeaderList = [];
  for (const [k, v] of headers) {
    const l = k.toLowerCase();
    if (RESPONSE_DROP.has(l)) continue;
    if (o.decoded && l === 'content-encoding') continue;
    if (l === 'location') out.push([k, rewriteLocation(v, o.target, o.contentOrigin)]);
    else if (l === 'content-security-policy') {
      const csp = v
        .split(';')
        .filter((d) => !/^\s*frame-ancestors(\s|$)/i.test(d))
        .join(';')
        .trim();
      if (csp) out.push([k, csp]);
    } else out.push([k, v]);
  }
  return out;
}

/** `http://localhost:5173/x` → `<content origin>/x`; other locations pass unchanged. */
export function rewriteLocation(location: string, target: string, contentOrigin: string): string {
  const port = target.split(':').pop();
  const hosts = [target, `127.0.0.1:${port}`, `[::1]:${port}`, `localhost:${port}`];
  for (const h of hosts) {
    for (const scheme of ['http://', 'https://']) {
      const base = `${scheme}${h}`;
      if (location === base) return `${contentOrigin}/`;
      if (location.startsWith(`${base}/`) || location.startsWith(`${base}?`) || location.startsWith(`${base}#`)) return contentOrigin + location.slice(base.length);
    }
  }
  return location;
}

/** All values of a header (case-insensitive). */
export const headerValues = (h: HeaderList, name: string): string[] => {
  const n = name.toLowerCase();
  return h.filter(([k]) => k.toLowerCase() === n).map(([, v]) => v);
};
export const headerValue = (h: HeaderList, name: string): string | null => headerValues(h, name)[0] ?? null;

// ---- response parser ----------------------------------------------------------------------

export interface ResponseHead {
  status: number;
  reason: string;
  headers: HeaderList;
}

export type ResponseEvent =
  | { type: 'head'; head: ResponseHead }
  | { type: 'data'; data: Uint8Array }
  | { type: 'end' }
  /** `101 Switching Protocols`: every later byte belongs to the new protocol (`rest` first). */
  | { type: 'upgrade'; head: ResponseHead; rest: Uint8Array };

export class HttpParseError extends Error {
  constructor(message: string) {
    super(message);
    this.name = 'HttpParseError';
  }
}

/** Longest response head (status line and headers). */
export const MAX_HEAD_BYTES = 64 * 1024;

type BodyMode = { k: 'none' } | { k: 'length'; left: number } | { k: 'chunked' } | { k: 'close' };

/**
 * Incremental parser for one HTTP/1.1 response. Feed bytes as they arrive (`push`), then `finish`
 * when the stream ends. Events come back from each call in order.
 */
export class ResponseParser {
  private buf: Uint8Array = new Uint8Array(0);
  private state: 'head' | 'body' | 'chunk-size' | 'chunk-data' | 'chunk-crlf' | 'trailers' | 'done' | 'upgraded' = 'head';
  private mode: BodyMode = { k: 'none' };
  private chunkLeft = 0;

  /** @param method the request method (`HEAD` responses have no body). */
  constructor(private readonly method = 'GET') {}

  get done(): boolean {
    return this.state === 'done' || this.state === 'upgraded';
  }

  push(bytes: Uint8Array): ResponseEvent[] {
    const out: ResponseEvent[] = [];
    if (this.state === 'done') return out;
    if (this.state === 'upgraded') {
      if (bytes.length) out.push({ type: 'data', data: bytes });
      return out;
    }
    this.buf = concat(this.buf, bytes);
    this.run(out);
    return out;
  }

  /** The stream ended. A close-delimited body ends here; anything else unfinished is an error. */
  finish(): ResponseEvent[] {
    if (this.state === 'done' || this.state === 'upgraded') return [];
    if (this.state === 'body' && this.mode.k === 'close') {
      this.state = 'done';
      return [{ type: 'end' }];
    }
    throw new HttpParseError(this.state === 'head' ? 'connection closed before a response' : 'connection closed mid-body');
  }

  private run(out: ResponseEvent[]): void {
    for (;;) {
      switch (this.state) {
        case 'head': {
          const end = indexOfCrlfCrlf(this.buf);
          if (end < 0) {
            if (this.buf.length > MAX_HEAD_BYTES) throw new HttpParseError('response head too large');
            return;
          }
          const head = parseHead(latin1.decode(this.buf.subarray(0, end)));
          this.buf = this.buf.subarray(end + 4);
          if (head.status === 101) {
            this.state = 'upgraded';
            out.push({ type: 'upgrade', head, rest: this.buf });
            this.buf = new Uint8Array(0);
            return;
          }
          if (head.status >= 100 && head.status < 200) continue; // 100 Continue, 103 Early Hints
          out.push({ type: 'head', head });
          this.mode = bodyMode(head, this.method);
          if (this.mode.k === 'none' || (this.mode.k === 'length' && this.mode.left === 0)) {
            this.state = 'done';
            out.push({ type: 'end' });
            return;
          }
          this.state = this.mode.k === 'chunked' ? 'chunk-size' : 'body';
          continue;
        }
        case 'body': {
          if (!this.buf.length) return;
          if (this.mode.k === 'length') {
            const n = Math.min(this.mode.left, this.buf.length);
            out.push({ type: 'data', data: this.buf.subarray(0, n) });
            this.buf = this.buf.subarray(n);
            this.mode.left -= n;
            if (this.mode.left === 0) {
              this.state = 'done';
              out.push({ type: 'end' });
              return;
            }
          } else {
            out.push({ type: 'data', data: this.buf });
            this.buf = new Uint8Array(0);
          }
          return;
        }
        case 'chunk-size': {
          const i = indexOfCrlf(this.buf);
          if (i < 0) {
            if (this.buf.length > 4096) throw new HttpParseError('chunk size line too long');
            return;
          }
          const line = latin1.decode(this.buf.subarray(0, i)).split(';')[0]!.trim();
          this.buf = this.buf.subarray(i + 2);
          if (!/^[0-9a-fA-F]{1,8}$/.test(line)) throw new HttpParseError('bad chunk size');
          this.chunkLeft = parseInt(line, 16);
          this.state = this.chunkLeft === 0 ? 'trailers' : 'chunk-data';
          continue;
        }
        case 'chunk-data': {
          if (!this.buf.length) return;
          const n = Math.min(this.chunkLeft, this.buf.length);
          out.push({ type: 'data', data: this.buf.subarray(0, n) });
          this.buf = this.buf.subarray(n);
          this.chunkLeft -= n;
          if (this.chunkLeft === 0) this.state = 'chunk-crlf';
          continue;
        }
        case 'chunk-crlf': {
          if (this.buf.length < 2) return;
          if (this.buf[0] !== 13 || this.buf[1] !== 10) throw new HttpParseError('missing CRLF after chunk');
          this.buf = this.buf.subarray(2);
          this.state = 'chunk-size';
          continue;
        }
        case 'trailers': {
          const i = indexOfCrlf(this.buf);
          if (i < 0) {
            if (this.buf.length > MAX_HEAD_BYTES) throw new HttpParseError('trailers too large');
            return;
          }
          this.buf = this.buf.subarray(i + 2);
          if (i === 0) {
            this.state = 'done';
            out.push({ type: 'end' });
            return;
          }
          continue; // a trailer field: ignored
        }
        default:
          return;
      }
    }
  }
}

function bodyMode(head: ResponseHead, method: string): BodyMode {
  if (method === 'HEAD' || head.status === 204 || head.status === 304) return { k: 'none' };
  const te = headerValues(head.headers, 'transfer-encoding').join(',').toLowerCase();
  if (te) {
    if (te.split(',').map((s) => s.trim()).pop() === 'chunked') return { k: 'chunked' };
    return { k: 'close' };
  }
  const cl = headerValues(head.headers, 'content-length');
  if (cl.length) {
    const vals = new Set(cl.flatMap((v) => v.split(',').map((s) => s.trim())));
    if (vals.size !== 1) throw new HttpParseError('conflicting content-length');
    const v = [...vals][0]!;
    if (!/^\d{1,15}$/.test(v)) throw new HttpParseError('bad content-length');
    return { k: 'length', left: Number(v) };
  }
  return { k: 'close' };
}

function parseHead(text: string): ResponseHead {
  const lines = text.split('\r\n');
  const status = /^HTTP\/1\.[01] (\d{3})(?: (.*))?$/.exec(lines[0] ?? '');
  if (!status) throw new HttpParseError('bad status line');
  const headers: HeaderList = [];
  for (const line of lines.slice(1)) {
    if (!line) continue;
    if (line[0] === ' ' || line[0] === '\t') {
      // obsolete line folding: append to the previous header
      const last = headers[headers.length - 1];
      if (!last) throw new HttpParseError('bad header continuation');
      last[1] += ` ${line.trim()}`;
      continue;
    }
    const i = line.indexOf(':');
    if (i <= 0) throw new HttpParseError('bad header line');
    const k = line.slice(0, i);
    if (!TOKEN.test(k)) throw new HttpParseError('bad header name');
    headers.push([k, line.slice(i + 1).trim()]);
  }
  return { status: Number(status[1]), reason: status[2] ?? '', headers };
}

function indexOfCrlf(b: Uint8Array): number {
  for (let i = 0; i + 1 < b.length; i++) if (b[i] === 13 && b[i + 1] === 10) return i;
  return -1;
}

function indexOfCrlfCrlf(b: Uint8Array): number {
  for (let i = 0; i + 3 < b.length; i++) if (b[i] === 13 && b[i + 1] === 10 && b[i + 2] === 13 && b[i + 3] === 10) return i;
  return -1;
}

export function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  if (!a.length) return b;
  if (!b.length) return a;
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}
