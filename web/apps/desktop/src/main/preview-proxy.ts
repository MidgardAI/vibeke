// The in-app HTTP proxy for embedded previews (spec 06 B3.5). Preview sessions send every request
// here (`setProxy`, bypass `<-loopback>`); each view authenticates with its own random credential,
// which selects the host and machine to route to.
//
// - `CONNECT host:port` (HTTPS, WebSockets) and absolute-form HTTP (`GET http://host/…`, forwarded
//   in origin form with `Connection: close`) are accepted; anything without a valid
//   `Proxy-Authorization: Basic` gets 407, so other local processes cannot use the proxy.
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

export interface PreviewRoute {
  hostId: string;
  /** Preview machine ('' = the host's own machine). */
  machine: string;
}

export type Destination = { kind: 'tunnel'; host: string; port: number } | { kind: 'direct'; address: string; port: number };

export interface ProxyDeps {
  /** A byte stream to `host:port` on the route's preview machine (gateway `tunnel.open`). */
  tunnel(route: PreviewRoute, host: string, port: number): Promise<Duplex>;
  /** A direct TCP connection from this machine (default: `net.connect`). Never a loopback address. */
  direct?(address: string, port: number): Promise<Duplex>;
  /** Name resolution (default: `dns.lookup` with `all: true`). */
  lookup?(host: string): Promise<{ address: string; family: number }[]>;
  log?(msg: string): void;
}

export const PROXY_REALM = 'vibeke-preview';
const MAX_HEAD = 64 * 1024;
const HEAD_TIMEOUT_MS = 30_000;
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
 * headers removed, `Connection: close` (or kept `Upgrade` for WebSocket handshakes).
 */
export function originRequest(h: RequestHead): { host: string; port: number; head: string } | null {
  let u: URL;
  try {
    u = new URL(h.target);
  } catch {
    return null;
  }
  if (u.protocol !== 'http:' || u.username || u.password || !u.hostname) return null;
  const port = u.port ? Number(u.port) : 80;
  const connTokens = (header(h, 'connection') ?? '')
    .split(',')
    .map((s) => s.trim().toLowerCase())
    .filter(Boolean);
  const upgrade = header(h, 'upgrade');
  const isUpgrade = !!upgrade && connTokens.includes('upgrade');
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
  else lines.push('Connection: close');
  return { host: normalizeHost(u.hostname), port, head: `${lines.join('\r\n')}\r\n\r\n` };
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
    const m = /^Basic\s+([A-Za-z0-9+/=]{1,512})$/i.exec(value?.trim() ?? '');
    if (!m) return null;
    const decoded = Buffer.from(m[1]!, 'base64').toString('utf8');
    const i = decoded.indexOf(':');
    if (i <= 0) return null;
    const c = this.creds.get(decoded.slice(0, i));
    if (!c) return null;
    const given = Buffer.from(decoded.slice(i + 1), 'utf8');
    if (given.length !== c.password.length || !timingSafeEqual(given, c.password)) return null;
    return c.route;
  }

  private onConnection(sock: Socket): void {
    this.sockets.add(sock);
    sock.on('close', () => this.sockets.delete(sock));
    sock.on('error', () => {});
    sock.setNoDelay(true);
    let buf: Buffer = Buffer.alloc(0);
    let headDone = false;
    const timer = setTimeout(() => sock.destroy(), HEAD_TIMEOUT_MS);
    const onData = (d: Buffer) => {
      buf = buf.length ? Buffer.concat([buf, d]) : d;
      const end = buf.indexOf('\r\n\r\n');
      if (end < 0) {
        if (buf.length > MAX_HEAD) {
          headDone = true;
          sock.off('data', onData);
          clearTimeout(timer);
          reply(sock, 431, 'request head too large');
        }
        return;
      }
      headDone = true;
      sock.off('data', onData);
      sock.pause();
      clearTimeout(timer);
      if (end > MAX_HEAD) return reply(sock, 431, 'request head too large');
      const head = parseHead(buf.subarray(0, end).toString('latin1'));
      void this.handle(sock, head, buf.subarray(end + 4)).catch((e) => {
        this.deps.log?.(`preview proxy: ${(e as Error).message}`);
        sock.destroy();
      });
    };
    sock.on('data', onData);
    sock.once('end', () => {
      if (headDone) return;
      clearTimeout(timer);
      sock.destroy();
    });
  }

  private async handle(sock: Socket, head: RequestHead | null, rest: Buffer): Promise<void> {
    if (!head) return reply(sock, 400, 'malformed request');
    const route = this.authenticate(header(head, 'proxy-authorization'));
    if (!route) return reply(sock, 407, 'proxy authentication required', [`Proxy-Authenticate: Basic realm="${PROXY_REALM}"`]);

    let target: { host: string; port: number };
    let forward: string | null = null;
    if (head.method === 'CONNECT') {
      const a = parseAuthority(head.target);
      if (!a) return reply(sock, 400, 'invalid CONNECT target');
      target = a;
    } else {
      const o = originRequest(head);
      if (!o) return reply(sock, 400, 'only absolute-form http:// requests are proxied');
      target = o;
      forward = o.head;
    }

    let upstream: Duplex;
    try {
      const d = await resolveDestination(target.host, target.port, this.deps.lookup);
      upstream = d.kind === 'tunnel' ? await this.deps.tunnel(route, d.host, d.port) : await (this.deps.direct ?? connectDirect)(d.address, d.port);
    } catch (e) {
      return reply(sock, 502, `cannot reach ${target.host}:${target.port}: ${(e as Error).message}`);
    }
    if (sock.destroyed) {
      upstream.destroy();
      return;
    }
    if (forward === null) sock.write('HTTP/1.1 200 Connection Established\r\n\r\n');
    else upstream.write(Buffer.from(forward, 'latin1'));
    if (rest.length) upstream.write(rest);
    splice(sock, upstream);
  }
}

/** Does an Electron `login` auth request come from this proxy? */
export function isOurProxyChallenge(authInfo: { isProxy: boolean; host: string; port: number; realm?: string }, port: number): boolean {
  return authInfo.isProxy && port > 0 && authInfo.port === port && normalizeHost(authInfo.host) === '127.0.0.1';
}
