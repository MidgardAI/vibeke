// The in-app HTTP proxy for embedded previews (spec 06 B3.5). Preview sessions send every request
// here (`setProxy`, bypass `<-loopback>`); each view authenticates with its own random credential,
// which selects the host and machine to route to.
//
// - `CONNECT host:port` (HTTPS, WebSockets) and absolute-form HTTP (`GET http://host/…`, forwarded
//   in origin form) are accepted; anything without a valid `Proxy-Authorization: Basic` gets 407,
//   so other local processes cannot use the proxy.
// - Plain HTTP keeps connections alive: the proxy finds where each response ends
//   (http-framing.ts), puts the upstream connection (a tunnel stream) back into a small pool per
//   view route and destination, and reads the next request from the same client connection. A
//   page with 100 scripts opens a few tunnel streams, not 100.
// - Loopback destinations (`localhost`, `*.localhost`, 127/8, ::1, and names that resolve only to
//   loopback addresses) go through a tunnel stream to the preview machine. Everything else
//   connects directly from this machine, never to this machine's own loopback.
//
// A plain `net` server parses the request head itself so the bytes after it pass through
// untouched (request bodies, WebSocket upgrades). Electron-free for unit tests.

import { randomBytes, timingSafeEqual } from 'node:crypto';
import { lookup as dnsLookup } from 'node:dns/promises';
import { connect as netConnect, createServer, isIP, type Server, type Socket } from 'node:net';
import type { Duplex } from 'node:stream';
import { ByteReader, copyBody, keepAlive, parseResponseHead, readHead, requestFraming, responseFraming, writeAll, type Framing } from './http-framing';

export interface PreviewRoute {
  hostId: string;
  /** Preview machine ('' = the host's own machine). */
  machine: string;
}

export type Destination = { kind: 'tunnel'; host: string; port: number } | { kind: 'direct'; address: string; port: number };

export interface ProxyDeps {
  /**
   * A byte stream to `host:port` on the route's preview machine (gateway `tunnel.open`). `view` is
   * the credential's user name: the view whose request needs it.
   */
  tunnel(route: PreviewRoute, host: string, port: number, view: string): Promise<Duplex>;
  /** A direct TCP connection from this machine (default: `net.connect`). Never a loopback address. */
  direct?(address: string, port: number): Promise<Duplex>;
  /** Name resolution (default: `dns.lookup` with `all: true`). */
  lookup?(host: string): Promise<{ address: string; family: number }[]>;
  log?(msg: string): void;
}

export const PROXY_REALM = 'vibeke-preview';
const MAX_HEAD = 64 * 1024;
const HEAD_TIMEOUT_MS = 30_000;
/** A kept-alive client connection waits this long for its next request. */
const CLIENT_IDLE_MS = 60_000;
/** An idle upstream connection stays in the pool this long. */
const POOL_IDLE_MS = 30_000;
/** Idle upstream connections per route and destination. */
const POOL_PER_KEY = 6;
const DIRECT_TIMEOUT_MS = 15_000;
/** After the upstream closed, the client side gets this long to flush before it is cut. */
const CLOSE_GRACE_MS = 10_000;

// ---- destinations ------------------------------------------------------------------------------

/** `[::1]` → `::1`, lower case, no trailing dot. */
export function normalizeHost(h: string): string {
  let s = h.trim().toLowerCase();
  if (s.startsWith('[') && s.endsWith(']')) s = s.slice(1, -1);
  return s.replace(/\.$/, '');
}

/**
 * The address to dial on the preview machine for a loopback (or unspecified) IP literal, or null
 * for any other address. IPv4-mapped loopback becomes plain IPv4; `0.0.0.0` / `::` (which reach
 * this machine when dialed) count as loopback too.
 */
export function loopbackIp(addr: string): string | null {
  const a = normalizeHost(addr);
  const v = isIP(a);
  if (v === 4) {
    if (a.startsWith('127.')) return a;
    return a === '0.0.0.0' ? '127.0.0.1' : null;
  }
  if (v !== 6) return null;
  let canon: string;
  try {
    canon = new URL(`http://[${a}]/`).hostname.slice(1, -1);
  } catch {
    return null;
  }
  if (canon === '::1') return '::1';
  if (canon === '::') return '::1';
  const mapped = /^::ffff:([0-9a-f]{1,4}):([0-9a-f]{1,4})$/.exec(canon);
  if (mapped) {
    const hi = parseInt(mapped[1]!, 16);
    const lo = parseInt(mapped[2]!, 16);
    return loopbackIp(`${hi >> 8}.${hi & 255}.${lo >> 8}.${lo & 255}`);
  }
  return null;
}

/** A loopback name or loopback IP literal (no resolution). Used for certificate leniency too. */
export function isLoopbackName(host: string): boolean {
  const h = normalizeHost(host);
  if (h === 'localhost' || h.endsWith('.localhost')) return true;
  return isIP(h) !== 0 && loopbackIp(h) !== null;
}

/** Same as `isLoopbackName` for a URL's host (false for unparsable URLs). */
export function isLoopbackUrl(url: string): boolean {
  try {
    return isLoopbackName(new URL(url).hostname);
  } catch {
    return false;
  }
}

const HOST_RE = /^[a-z0-9_.:%-]{1,253}$/;

const defaultLookup = (host: string) => dnsLookup(host, { all: true, verbatim: true });

/** Where a request for `host:port` goes. Names resolve once, here, so the dial cannot rebind. */
export async function resolveDestination(host: string, port: number, lookup = defaultLookup): Promise<Destination> {
  const h = normalizeHost(host);
  if (!HOST_RE.test(h)) throw new Error('invalid host');
  if (!Number.isInteger(port) || port < 1 || port > 65535) throw new Error('invalid port');
  if (h === 'localhost' || h.endsWith('.localhost')) return { kind: 'tunnel', host: h, port };
  if (isIP(h)) {
    const lb = loopbackIp(h);
    return lb ? { kind: 'tunnel', host: lb, port } : { kind: 'direct', address: h, port };
  }
  const addrs = await lookup(h);
  if (!addrs.length) throw new Error(`${h} did not resolve`);
  const outside = addrs.find((a) => loopbackIp(a.address) === null);
  if (outside) return { kind: 'direct', address: outside.address, port };
  // Resolves only to loopback: that loopback is the preview machine's, never this machine's.
  return { kind: 'tunnel', host: loopbackIp(addrs[0]!.address)!, port };
}

function connectDirect(address: string, port: number): Promise<Duplex> {
  if (loopbackIp(address) !== null) return Promise.reject(new Error('refusing a direct loopback connection'));
  return new Promise((resolve, reject) => {
    const s = netConnect({ host: address, port });
    const timer = setTimeout(() => s.destroy(new Error('connect timeout')), DIRECT_TIMEOUT_MS);
    s.once('connect', () => {
      clearTimeout(timer);
      s.off('error', reject);
      s.setNoDelay(true);
      resolve(s);
    });
    s.once('error', (e) => {
      clearTimeout(timer);
      reject(e);
    });
  });
}

// ---- request heads -----------------------------------------------------------------------------

export interface RequestHead {
  method: string;
  target: string;
  version: string;
  headers: [string, string][];
}

/** Parses an HTTP/1.x request head (without the blank line). Null when malformed. */
export function parseHead(text: string): RequestHead | null {
  const lines = text.split('\r\n');
  const m = /^([A-Z]{1,20}) (\S{1,8192}) (HTTP\/1\.[01])$/.exec(lines[0] ?? '');
  if (!m) return null;
  const headers: [string, string][] = [];
  for (const line of lines.slice(1)) {
    const i = line.indexOf(':');
    if (i <= 0) return null;
    const name = line.slice(0, i);
    if (!/^[!#$%&'*+.^_`|~0-9A-Za-z-]+$/.test(name)) return null;
    headers.push([name, line.slice(i + 1).trim()]);
  }
  return { method: m[1]!, target: m[2]!, version: m[3]!, headers };
}

const header = (h: RequestHead, name: string): string | undefined => h.headers.find(([k]) => k.toLowerCase() === name)?.[1];

/** `host:port` / `[v6]:port` of a CONNECT request. */
export function parseAuthority(target: string): { host: string; port: number } | null {
  const m = /^(\[[0-9a-fA-F:.%]+\]|[^:[\]]+):(\d{1,5})$/.exec(target);
  if (!m) return null;
  const port = Number(m[2]);
  if (port < 1 || port > 65535) return null;
  return { host: normalizeHost(m[1]!), port };
}

const HOP_BY_HOP = new Set(['proxy-authorization', 'proxy-connection', 'proxy-authenticate', 'connection', 'keep-alive', 'te', 'trailer', 'upgrade']);

/**
 * An absolute-form request rewritten for the origin: origin-form target, proxy and hop-by-hop
 * headers removed. The connection stays open for HTTP/1.1 (the proxy pools it), except when the
 * client asked to close it; WebSocket handshakes keep their `Upgrade`.
 */
export function originRequest(h: RequestHead): { host: string; port: number; head: string; upgrade: boolean; close: boolean } | null {
  let u: URL;
  try {
    u = new URL(h.target);
  } catch {
    return null;
  }
  if (u.protocol !== 'http:' || u.username || u.password || !u.hostname) return null;
  const port = u.port ? Number(u.port) : 80;
  const connTokens = (header(h, 'connection') ?? header(h, 'proxy-connection') ?? '')
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  const upgrade = header(h, 'upgrade');
  const isUpgrade = !!upgrade && connTokens.includes('upgrade');
  const close = h.version === 'HTTP/1.0' ? !connTokens.includes('keep-alive') : connTokens.includes('close');
  const drop = new Set([...HOP_BY_HOP, ...connTokens]);
  const lines = [`${h.method} ${u.pathname || '/'}${u.search} ${h.version}`];
  let hasHost = false;
  for (const [k, v] of h.headers) {
    const lk = k.toLowerCase();
    if (drop.has(lk)) continue;
    if (lk === 'host') hasHost = true;
    lines.push(`${k}: ${v}`);
  }
  if (!hasHost) lines.push(`Host: ${u.host}`);
  if (isUpgrade) lines.push(`Upgrade: ${upgrade}`, 'Connection: Upgrade');
  else if (close) lines.push('Connection: close');
  return { host: normalizeHost(u.hostname), port, head: `${lines.join('\r\n')}\r\n\r\n`, upgrade: isUpgrade, close: close && !isUpgrade };
}

// ---- the proxy ---------------------------------------------------------------------------------

interface Credential {
  password: Buffer;
  route: PreviewRoute;
}

const STATUS_TEXT: Record<number, string> = { 400: 'Bad Request', 407: 'Proxy Authentication Required', 431: 'Request Header Fields Too Large', 502: 'Bad Gateway' };

function reply(sock: Socket, code: number, body: string, extra: string[] = []): void {
  if (sock.destroyed) return;
  const b = Buffer.from(body, 'utf8');
  const head = [`HTTP/1.1 ${code} ${STATUS_TEXT[code] ?? 'Error'}`, 'Content-Type: text/plain; charset=utf-8', `Content-Length: ${b.length}`, 'Connection: close', 'Proxy-Connection: close', ...extra];
  sock.end(Buffer.concat([Buffer.from(`${head.join('\r\n')}\r\n\r\n`, 'latin1'), b]));
}

/** Pipes two streams together; when one side closes, the other ends (and is cut after a grace). */
function splice(client: Socket, upstream: Duplex): void {
  const killAll = () => {
    client.destroy();
    upstream.destroy();
  };
  client.on('error', killAll);
  upstream.on('error', killAll);
  client.on('close', () => upstream.destroy());
  upstream.on('close', () => {
    if (client.destroyed) return;
    client.end();
    setTimeout(() => client.destroy(), CLOSE_GRACE_MS).unref?.();
  });
  client.pipe(upstream);
  upstream.pipe(client);
  client.resume();
}

export class PreviewProxy {
  private server: Server | null = null;
  private listening: Promise<number> | null = null;
  private readonly creds = new Map<string, Credential>();
  private readonly sockets = new Set<Socket>();
  private readonly pool = new Map<string, Upstream[]>();
  port = 0;

  constructor(private readonly deps: ProxyDeps) {}

  /** Listens on 127.0.0.1 (ephemeral port) once; resolves with the port. */
  start(): Promise<number> {
    this.listening ??= new Promise<number>((resolve, reject) => {
      const server = createServer({ allowHalfOpen: true }, (s) => this.onConnection(s));
      server.maxConnections = 1024;
      server.once('error', reject);
      server.listen({ host: '127.0.0.1', port: 0, exclusive: true }, () => {
        const a = server.address();
        this.port = typeof a === 'object' && a ? a.port : 0;
        this.server = server;
        server.off('error', reject);
        server.on('error', (e) => this.deps.log?.(`preview proxy: ${e.message}`));
        resolve(this.port);
      });
    });
    this.listening.catch(() => (this.listening = null));
    return this.listening;
  }

  close(): void {
    this.server?.close();
    for (const s of this.sockets) s.destroy();
    this.sockets.clear();
    for (const list of this.pool.values()) for (const up of list) up.stream.destroy();
    this.pool.clear();
    this.server = null;
    this.listening = null;
  }

  /** A fresh credential for a view: the username is the view id, the password 32 random bytes. */
  addView(view: string, route: PreviewRoute): { username: string; password: string } {
    if (!/^[A-Za-z0-9_-]{1,64}$/.test(view)) throw new Error('invalid view id');
    const password = randomBytes(32).toString('base64url');
    this.creds.set(view, { password: Buffer.from(password, 'utf8'), route: { ...route } });
    return { username: view, password };
  }

  removeView(view: string): void {
    this.creds.delete(view);
  }

  /** The route of a valid `Proxy-Authorization: Basic …` value, else null (constant-time compare). */
  authenticate(value: string | undefined): PreviewRoute | null {
    return this.authenticateView(value)?.route ?? null;
  }

  /** The view (credential user name) and route of a valid `Proxy-Authorization` value. */
  authenticateView(value: string | undefined): { view: string; route: PreviewRoute } | null {
    const m = /^Basic\s+([A-Za-z0-9+/=]{1,512})$/i.exec(value?.trim() ?? '');
    if (!m) return null;
    const decoded = Buffer.from(m[1]!, 'base64').toString('utf8');
    const i = decoded.indexOf(':');
    if (i <= 0) return null;
    const view = decoded.slice(0, i);
    const c = this.creds.get(view);
    if (!c) return null;
    const given = Buffer.from(decoded.slice(i + 1), 'utf8');
    if (given.length !== c.password.length || !timingSafeEqual(given, c.password)) return null;
    return { view, route: c.route };
  }

  /** Idle upstream connections in the pool (for tests). */
  get pooled(): number {
    let n = 0;
    for (const l of this.pool.values()) n += l.length;
    return n;
  }

  private onConnection(sock: Socket): void {
    this.sockets.add(sock);
    sock.on('close', () => this.sockets.delete(sock));
    sock.on('error', () => {});
    sock.setNoDelay(true);
    void this.serve(sock).catch((e) => {
      this.deps.log?.(`preview proxy: ${(e as Error).message}`);
      sock.destroy();
    });
  }

  /** One client connection: requests one after another until it closes or upgrades. */
  private async serve(sock: Socket): Promise<void> {
    const client = new ByteReader(sock);
    for (let first = true; ; first = false) {
      const got = await readHead(client, MAX_HEAD, first ? HEAD_TIMEOUT_MS : CLIENT_IDLE_MS);
      if ('error' in got) {
        if (got.error === 'too_large') return reply(sock, 431, 'request head too large');
        sock.destroy();
        return;
      }
      const head = parseHead(got.head.toString('latin1'));
      if (!head) return reply(sock, 400, 'malformed request');
      const auth = this.authenticateView(header(head, 'proxy-authorization'));
      if (!auth) return reply(sock, 407, 'proxy authentication required', [`Proxy-Authenticate: Basic realm="${PROXY_REALM}"`]);
      const { view, route } = auth;

      if (head.method === 'CONNECT') {
        const a = parseAuthority(head.target);
        if (!a) return reply(sock, 400, 'invalid CONNECT target');
        const up = await this.dial(sock, route, view, a.host, a.port);
        if (!up) return;
        sock.write('HTTP/1.1 200 Connection Established\r\n\r\n');
        return this.spliceRest(sock, client, up.stream, up.reader?.detach());
      }

      const o = originRequest(head);
      if (!o) return reply(sock, 400, 'only absolute-form http:// requests are proxied');
      if (o.upgrade) {
        const up = await this.dial(sock, route, view, o.host, o.port);
        if (!up) return;
        await writeAll(up.stream, Buffer.from(o.head, 'latin1'));
        return this.spliceRest(sock, client, up.stream, up.reader?.detach());
      }
      const framing = requestFraming(head.headers);
      if (!framing) return reply(sock, 400, 'invalid request body framing');
      const keep = await this.exchange(sock, client, route, view, head.method, o, framing);
      if (!keep) return;
    }
  }

  /** Dial `host:port` for a route, or answer 502 and return null. */
  private async dial(sock: Socket, route: PreviewRoute, view: string, host: string, port: number): Promise<Upstream | null> {
    let up: Upstream;
    try {
      const d = await resolveDestination(host, port, this.deps.lookup);
      const stream = d.kind === 'tunnel' ? await this.deps.tunnel(route, d.host, d.port, view) : await (this.deps.direct ?? connectDirect)(d.address, d.port);
      up = { stream, reader: null, key: poolKey(route, d) };
    } catch (e) {
      reply(sock, 502, `cannot reach ${host}:${port}: ${(e as Error).message}`);
      return null;
    }
    if (sock.destroyed) {
      up.stream.destroy();
      return null;
    }
    return up;
  }

  /** Raw two-way pipe for CONNECT, upgrades and non-HTTP answers. */
  private spliceRest(sock: Socket, client: ByteReader, upstream: Duplex, upBuffered?: Buffer): void {
    const rest = client.detach();
    if (rest.length) upstream.write(rest);
    if (upBuffered?.length) sock.write(upBuffered);
    splice(sock, upstream);
  }

  /**
   * One request and its response over a pooled (or new) upstream connection. Returns true when the
   * client connection may carry another request.
   */
  private async exchange(
    sock: Socket,
    client: ByteReader,
    route: PreviewRoute,
    view: string,
    method: string,
    o: { host: string; port: number; head: string; close: boolean },
    framing: Framing,
  ): Promise<boolean> {
    let d: Destination;
    try {
      d = await resolveDestination(o.host, o.port, this.deps.lookup);
    } catch (e) {
      reply(sock, 502, `cannot reach ${o.host}:${o.port}: ${(e as Error).message}`);
      return false;
    }
    const key = poolKey(route, d);
    const headBuf = Buffer.from(o.head, 'latin1');
    // A pooled connection the server already closed fails before any answer: a request without
    // a body is sent once more on a fresh connection.
    for (let attempt = 0; ; attempt++) {
      if (sock.destroyed) return false;
      let up = attempt === 0 ? this.take(key) : null;
      const pooled = !!up;
      if (!up) {
        const fresh = await this.dial(sock, route, view, o.host, o.port);
        if (!fresh) return false;
        up = fresh;
      }
      const reader = (up.reader ??= new ByteReader(up.stream));
      const upstream = up.stream;
      const cut = () => upstream.destroy();
      sock.once('close', cut);
      try {
        try {
          await writeAll(upstream, headBuf);
          await copyBody(client, framing, upstream);
        } catch (e) {
          upstream.destroy();
          if (pooled && framing.kind === 'none') continue;
          throw e;
        }
        const r = await this.relayResponse(sock, reader, method);
        if (r === 'empty') {
          upstream.destroy();
          if (pooled && framing.kind === 'none') continue;
          reply(sock, 502, `${o.host}:${o.port} closed the connection without an answer`);
          return false;
        }
        if (r === 'raw') {
          sock.off('close', cut);
          this.spliceRest(sock, client, upstream, reader.detach());
          return false;
        }
        if (r.reusable && !reader.buffered && !upstream.destroyed) this.put(key, up);
        else upstream.destroy();
        if (r.closeClient || o.close) {
          sock.end();
          return false;
        }
        return true;
      } finally {
        sock.off('close', cut);
      }
    }
  }

  /**
   * Copy one response (after any 1xx) to the client. `empty`: the upstream closed before sending
   * anything; `raw`: not HTTP (or an upgrade), the caller splices the connection.
   */
  private async relayResponse(sock: Socket, r: ByteReader, method: string): Promise<'empty' | 'raw' | { reusable: boolean; closeClient: boolean }> {
    for (let first = true; ; first = false) {
      const got = await readHead(r, MAX_HEAD);
      if ('error' in got) {
        if (got.error === 'closed' && first) return 'empty';
        throw new Error(`upstream response: ${got.error}`);
      }
      const res = parseResponseHead(got.head.toString('latin1'));
      if (!res) {
        // Not an HTTP answer: pass it through untouched.
        r.unread(Buffer.concat([got.head, Buffer.from('\r\n\r\n', 'latin1')]));
        return 'raw';
      }
      await writeAll(sock, Buffer.concat([got.head, Buffer.from('\r\n\r\n', 'latin1')]));
      if (res.status === 101) return 'raw';
      if (res.status >= 100 && res.status < 200) continue;
      const framing = responseFraming(method, res);
      if (!framing) throw new Error('invalid response body framing');
      await copyBody(r, framing, sock);
      const persistent = keepAlive(res.version, res.headers) && framing.kind !== 'close';
      return { reusable: persistent, closeClient: !persistent };
    }
  }

  private take(key: string): Upstream | null {
    const list = this.pool.get(key);
    while (list?.length) {
      const up = list.pop()!;
      if (!list.length) this.pool.delete(key);
      if (up.idle) clearTimeout(up.idle);
      up.stream.off('close', up.onIdleClose!);
      if (!up.stream.destroyed && !up.reader?.done) return up;
    }
    return null;
  }

  private put(key: string, up: Upstream): void {
    const list = this.pool.get(key) ?? [];
    if (list.length >= POOL_PER_KEY) {
      up.stream.destroy();
      return;
    }
    const drop = () => {
      const l = this.pool.get(key);
      const i = l?.indexOf(up) ?? -1;
      if (l && i >= 0) l.splice(i, 1);
      if (l && !l.length) this.pool.delete(key);
      up.stream.destroy();
    };
    up.onIdleClose = drop;
    up.idle = setTimeout(drop, POOL_IDLE_MS);
    (up.idle as { unref?: () => void }).unref?.();
    up.stream.once('close', drop);
    list.push(up);
    this.pool.set(key, list);
  }

  /** Drop pooled connections of a route (its view went away). */
  dropPool(route: PreviewRoute): void {
    const prefix = `${route.hostId}\0${route.machine}\0`;
    for (const [k, list] of [...this.pool]) {
      if (!k.startsWith(prefix)) continue;
      this.pool.delete(k);
      for (const up of list) {
        if (up.idle) clearTimeout(up.idle);
        up.stream.destroy();
      }
    }
  }
}

interface Upstream {
  stream: Duplex;
  reader: ByteReader | null;
  key: string;
  idle?: ReturnType<typeof setTimeout>;
  onIdleClose?: () => void;
}

const poolKey = (route: PreviewRoute, d: Destination): string =>
  `${route.hostId}\0${route.machine}\0${d.kind}\0${d.kind === 'tunnel' ? d.host : d.address}\0${d.port}`;

/** Does an Electron `login` auth request come from this proxy? */
export function isOurProxyChallenge(authInfo: { isProxy: boolean; host: string; port: number; realm?: string }, port: number): boolean {
  return authInfo.isProxy && port > 0 && authInfo.port === port && normalizeHost(authInfo.host) === '127.0.0.1';
}
