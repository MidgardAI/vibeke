// The review controller (spec 20 §3.6): runs on the app origin, holds the keys, the Noise channel
// and the cookie jar, and serves the preview that runs on the content origin
// (`https://<sid>.p.<domain>`) through the gateway's tunnel.
//
// Boot: a hidden bridge frame (`/__vk/bridge`) registers the service worker and posts `ready`; the
// controller hands it a MessagePort; the bridge relays one channel per worker (start, restarts).
// The visible frame then loads the preview path; the worker forwards every request as `fetch`.
// The page's injected script (`/__vk/inject.js`) sends `cookies.get` with its own port, then
// `ws.*`, `pick` and `cookies.*` over it.
//
// Every message from the content origin is checked (core `review-protocol.ts`) before use. A
// request can only reach the one preview the review device may reach anyway.

import {
  CookieJar,
  FrameReader,
  OP,
  ResponseParser,
  TunnelClient,
  closePayload,
  encodeFrame,
  forwardRequestHeaders,
  forwardResponseHeaders,
  headerValue,
  headerValues,
  parseBridgeMessage,
  parsePageMessage,
  parseWorkerMessage,
  wrap,
  writeRequest,
  wsAccept,
  wsKey,
  type FetchMsg,
  type PickPayload,
  type ResponseEvent,
  type RpcLink,
  type ToPage,
  type ToWorker,
  type TunnelConn,
  type WsOpenMsg,
} from '@vibeke/core';

/** At most this many tunnel streams at once per review (spec 20 §3.6 limits). */
export const MAX_STREAMS = 32;
/** The bridge must say `ready` within this time, or the screen falls back to the screencast. */
export const BOOT_TIMEOUT_MS = 15_000;

export interface ControllerOptions {
  contentOrigin: string;
  /** Preview id for `tunnel.open {preview}`. */
  preview: string;
  /** The dev server's authority, e.g. `localhost:5173` (Host header, Origin, Location). */
  authority: string;
  /** The first path the visible frame opens. */
  path: string;
  /** The live connection, or null while offline. */
  link(): RpcLink | null;
  jar: CookieJar;
  /** The jar changed: persist it (the caller debounces). */
  jarChanged(): void;
  now(): number;
  random(n: number): Uint8Array;
  onPick(p: PickPayload): void;
  onState(s: { phase: 'booting' | 'ready' | 'failed'; reason?: string }): void;
}

type Sink = { post(m: ToWorker, transfer?: Transferable[]): void };

/** A slot in the stream limit; released once. */
class Limiter {
  private used = 0;
  private waiting: (() => void)[] = [];
  constructor(private readonly max: number) {}
  async take(): Promise<() => void> {
    if (this.used >= this.max) await new Promise<void>((r) => this.waiting.push(r));
    this.used++;
    let done = false;
    return () => {
      if (done) return;
      done = true;
      this.used--;
      this.waiting.shift()?.();
    };
  }
}

interface WsState {
  conn: TunnelConn | null;
  open: boolean;
  closed: boolean;
  release: () => void;
}

export class ReviewController {
  private bridge: HTMLIFrameElement | null = null;
  private content: HTMLIFrameElement | null = null;
  private bridgePort: MessagePort | null = null;
  private workerPorts = new Set<MessagePort>();
  private pagePort: MessagePort | null = null;
  private pagePath = '/';
  private tunnels: TunnelClient | null = null;
  private tunnelLink: RpcLink | null = null;
  private limiter = new Limiter(MAX_STREAMS);
  private fetches = new Map<string, TunnelConn>();
  private sockets = new Map<number, WsState>();
  private annotating = false;
  private bootTimer: ReturnType<typeof setTimeout> | null = null;
  private disposed = false;
  private readonly onMessage = (e: MessageEvent) => this.windowMessage(e);

  constructor(private readonly o: ControllerOptions) {}

  /** Start: load the bridge; the content frame follows once the bridge is ready. */
  attach(bridge: HTMLIFrameElement, content: HTMLIFrameElement): void {
    this.bridge = bridge;
    this.content = content;
    window.addEventListener('message', this.onMessage);
    this.o.onState({ phase: 'booting' });
    this.bootTimer = setTimeout(() => this.o.onState({ phase: 'failed', reason: 'timeout' }), BOOT_TIMEOUT_MS);
    bridge.src = `${this.o.contentOrigin}/__vk/bridge`;
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    window.removeEventListener('message', this.onMessage);
    if (this.bootTimer) clearTimeout(this.bootTimer);
    for (const c of this.fetches.values()) c.close();
    this.fetches.clear();
    for (const s of this.sockets.values()) s.conn?.close();
    this.sockets.clear();
    this.bridgePort?.close();
    this.pagePort?.close();
    for (const p of this.workerPorts) p.close();
    this.workerPorts.clear();
    this.tunnels?.dispose();
    if (this.bridge) this.bridge.src = 'about:blank';
    if (this.content) this.content.src = 'about:blank';
  }

  setAnnotate(on: boolean): void {
    this.annotating = on;
    this.toPage({ t: 'annotate', on });
  }

  /** Reload the page where it is; without a page script, load the first path again. */
  reload(): void {
    if (this.pagePort) this.toPage({ t: 'reload' });
    else if (this.content) this.content.src = `${this.o.contentOrigin}${this.o.path}`;
  }

  // ---- window messages: the bridge's `ready`, the page's first `cookies.get` ------------------

  private windowMessage(e: MessageEvent): void {
    if (this.disposed || e.origin !== this.o.contentOrigin) return;
    if (this.bridge && e.source === this.bridge.contentWindow) {
      const m = parseBridgeMessage(e.data);
      if (m?.t === 'ready') this.bridgeReady();
      return;
    }
    if (this.content && e.source === this.content.contentWindow) {
      const m = parsePageMessage(e.data, this.o.contentOrigin);
      const port = e.ports[0];
      if (m?.t !== 'cookies.get' || !port) return;
      // A new page in the frame (navigation or reload): its sockets died with the old page.
      // Mark them closed and give back their stream slots, so a socket still opening its stream
      // closes it when it arrives and its late callbacks are ignored.
      this.pagePort?.close();
      for (const s of this.sockets.values()) this.drop(s);
      this.sockets.clear();
      this.pagePort = port;
      this.pagePath = m.path;
      port.onmessage = (ev) => this.pageMessage(port, ev);
      port.start();
      this.toPage({ t: 'cookies', visible: this.o.jar.visible(m.path, this.o.now()) });
      if (this.annotating) this.toPage({ t: 'annotate', on: true });
    }
  }

  private bridgeReady(): void {
    if (!this.bridge?.contentWindow) return;
    // A bridge reload (rare) gets a fresh port; old worker channels keep working.
    this.bridgePort?.close();
    const ch = new MessageChannel();
    this.bridgePort = ch.port1;
    ch.port1.onmessage = (ev) => {
      const m = parseBridgeMessage(ev.data);
      const port = ev.ports[0];
      if (m?.t === 'channel' && port) this.attachWorker(port);
    };
    ch.port1.start();
    this.bridge.contentWindow.postMessage(wrap({ t: 'port' }), this.o.contentOrigin, [ch.port2]);
    if (this.bootTimer) clearTimeout(this.bootTimer);
    this.bootTimer = null;
    if (this.content && (!this.content.src || this.content.src === 'about:blank')) this.content.src = `${this.o.contentOrigin}${this.o.path}`;
    this.o.onState({ phase: 'ready' });
  }

  private attachWorker(port: MessagePort): void {
    this.workerPorts.add(port);
    const sink: Sink = { post: (m, transfer) => port.postMessage(wrap(m), transfer ?? []) };
    port.onmessage = (ev) => {
      const m = parseWorkerMessage(ev.data, this.o.contentOrigin);
      if (!m) return;
      if (m.t === 'fetch') void this.fetch(m, sink, port);
      else this.fetches.get(fetchKey(port, m.id))?.close();
    };
    port.start();
  }

  private toPage(m: ToPage, transfer: Transferable[] = []): void {
    try {
      this.pagePort?.postMessage(wrap(m), transfer);
    } catch {
      // the page went away
    }
  }

  // ---- tunnel ----------------------------------------------------------------------------------

  private async openStream(): Promise<TunnelConn> {
    const link = this.o.link();
    if (!link) throw new Error('the host is not connected');
    if (!this.tunnels || this.tunnelLink !== link || this.tunnels.closed) {
      this.tunnels = new TunnelClient(link);
      this.tunnelLink = link;
    }
    return this.tunnels.open({ preview: this.o.preview });
  }

  private pathOf(url: string): string {
    return url.slice(this.o.contentOrigin.length) || '/';
  }

  // ---- fetch -----------------------------------------------------------------------------------

  private async fetch(m: FetchMsg, sink: Sink, port: MessagePort): Promise<void> {
    const key = fetchKey(port, m.id);
    const release = await this.limiter.take();
    let conn: TunnelConn | null = null;
    let finished = false;
    const fail = (message: string) => {
      if (finished) return;
      finished = true;
      sink.post({ t: 'fetch.error', id: m.id, message });
    };
    try {
      const target = this.pathOf(m.url);
      const pathname = target.split('?')[0]!;
      const now = this.o.now();
      const headers = forwardRequestHeaders(m.headers, { target: this.o.authority, contentOrigin: this.o.contentOrigin, cookie: this.o.jar.header(pathname, now) });
      const bytes = writeRequest({ method: m.method, target, headers }, m.body ? new Uint8Array(m.body) : null);
      conn = await this.openStream();
      this.fetches.set(key, conn);
      const parser = new ResponseParser(m.method);
      let body: BodyOut | null = null;
      const handle = (events: ResponseEvent[]) => {
        for (const ev of events) {
          if (ev.type === 'head') {
            const setCookies = headerValues(ev.head.headers, 'set-cookie');
            let changed = false;
            for (const sc of setCookies) changed = this.o.jar.setFromHeader(sc, this.o.now(), pathname) || changed;
            if (changed) {
              this.o.jarChanged();
              this.toPage({ t: 'cookies', visible: this.o.jar.visible(this.pagePath, this.o.now()) });
            }
            const encoding = (headerValue(ev.head.headers, 'content-encoding') ?? '').trim().toLowerCase();
            const decode = encoding === 'gzip' || encoding === 'x-gzip' ? 'gzip' : encoding === 'deflate' ? 'deflate' : null;
            const out = forwardResponseHeaders(ev.head.headers, { target: this.o.authority, contentOrigin: this.o.contentOrigin, decoded: !!decode });
            const html = (headerValue(ev.head.headers, 'content-type') ?? '').toLowerCase().includes('text/html');
            const head: Extract<ToWorker, { t: 'fetch.head' }> = { t: 'fetch.head', id: m.id, status: ev.head.status, statusText: ev.head.reason, headers: out };
            if (html) head.cookies = this.o.jar.visible(pathname, this.o.now());
            sink.post(head);
            body = new BodyOut(m.id, sink, decode);
          } else if (ev.type === 'data') {
            body?.write(ev.data);
          } else if (ev.type === 'end') {
            finished = true;
            const b = body;
            void (b ? b.end() : Promise.resolve(sink.post({ t: 'fetch.end', id: m.id })));
            conn?.close();
          } else if (ev.type === 'upgrade') {
            fail('unexpected upgrade');
            conn?.close();
          }
        }
      };
      conn.listen({
        data: (d) => {
          if (finished) return;
          try {
            handle(parser.push(d));
          } catch (e) {
            fail((e as Error).message);
            conn?.close();
          }
        },
        end: () => {
          if (finished) return;
          try {
            handle(parser.finish());
          } catch (e) {
            fail((e as Error).message);
          }
          conn?.close();
        },
        close: (reason) => {
          this.fetches.delete(key);
          release();
          if (!finished) {
            try {
              handle(parser.finish());
            } catch {
              fail(reason === 'local' ? 'aborted' : `the connection to the preview ended (${reason})`);
            }
          }
        },
      });
      await conn.write(bytes);
    } catch (e) {
      fail((e as Error).message);
      conn?.close();
      if (!conn) release();
    }
  }

  // ---- WebSockets ------------------------------------------------------------------------------

  private pageMessage(port: MessagePort, ev: MessageEvent): void {
    if (port !== this.pagePort) return;
    const m = parsePageMessage(ev.data, this.o.contentOrigin);
    if (!m) return;
    switch (m.t) {
      case 'cookies.get':
        this.pagePath = m.path;
        this.toPage({ t: 'cookies', visible: this.o.jar.visible(m.path, this.o.now()) });
        break;
      case 'cookies.set':
        this.pagePath = m.path;
        if (this.o.jar.setFromDocument(m.cookie, this.o.now(), m.path)) this.o.jarChanged();
        this.toPage({ t: 'cookies', visible: this.o.jar.visible(m.path, this.o.now()) });
        break;
      case 'pick':
        this.o.onPick({ ...m.pick, url: this.toPreviewUrl(m.pick.url) });
        break;
      case 'ws.open':
        void this.wsOpen(m);
        break;
      case 'ws.send': {
        const s = this.sockets.get(m.id);
        if (!s?.open || !s.conn) return;
        const payload = typeof m.data === 'string' ? new TextEncoder().encode(m.data) : new Uint8Array(m.data);
        s.conn.write(encodeFrame(typeof m.data === 'string' ? OP.text : OP.binary, payload, this.o.random(4))).catch(() => this.wsGoneIf(s, m.id, 1006, ''));
        break;
      }
      case 'ws.close': {
        const s = this.sockets.get(m.id);
        if (!s) return;
        if (s.open && s.conn) {
          s.conn.write(encodeFrame(OP.close, closePayload(m.code ?? 1000, m.reason ?? ''), this.o.random(4))).catch(() => {});
          // Bound to this socket: a newer page may reuse the id before the timer fires.
          setTimeout(() => this.wsGoneIf(s, m.id, m.code ?? 1000, m.reason ?? '', true), 2000);
        } else this.wsGone(m.id, 1006, '');
        break;
      }
    }
  }

  /** The page's URL as the preview's own (`http://localhost:5173/…`), for the host's crop. */
  private toPreviewUrl(url: string): string {
    return url.startsWith(this.o.contentOrigin) ? `http://${this.o.authority}${url.slice(this.o.contentOrigin.length) || '/'}` : url;
  }

  private async wsOpen(m: WsOpenMsg): Promise<void> {
    if (this.sockets.has(m.id)) return;
    const release = await this.limiter.take();
    const state: WsState = { conn: null, open: false, closed: false, release };
    this.sockets.set(m.id, state);
    // Callbacks of this socket only, never of a newer page's socket with the same id.
    const gone = (code: number, reason: string, clean = false) => {
      if (this.sockets.get(m.id) === state) this.wsGone(m.id, code, reason, clean);
      else this.drop(state);
    };
    try {
      const host = m.url.replace(/^wss?:\/\//, '');
      const target = host.slice(host.indexOf('/')) || '/';
      const key = wsKey((n) => this.o.random(n));
      const headers = forwardRequestHeaders([['Origin', this.o.contentOrigin]], {
        target: this.o.authority,
        contentOrigin: this.o.contentOrigin,
        cookie: this.o.jar.header(target.split('?')[0]!, this.o.now()),
        upgrade: true,
      });
      headers.push(['Sec-WebSocket-Version', '13'], ['Sec-WebSocket-Key', key]);
      if (m.protocols.length) headers.push(['Sec-WebSocket-Protocol', m.protocols.join(', ')]);
      const conn = await this.openStream();
      if (state.closed) return conn.close();
      state.conn = conn;
      const parser = new ResponseParser('GET');
      const frames = new FrameReader();
      const onFrames = (bytes: Uint8Array) => {
        for (const f of frames.push(bytes)) {
          if (state.closed) return;
          if (f.type === 'text') this.toPage({ t: 'ws.message', id: m.id, data: f.data });
          else if (f.type === 'binary') {
            const buf = f.data.slice().buffer as ArrayBuffer;
            this.toPage({ t: 'ws.message', id: m.id, data: buf }, [buf]);
          } else if (f.type === 'ping') conn.write(encodeFrame(OP.pong, f.data, this.o.random(4))).catch(() => {});
          else if (f.type === 'close') {
            conn.write(encodeFrame(OP.close, closePayload(f.code === 1005 ? undefined : f.code), this.o.random(4))).catch(() => {});
            gone(f.code === 1005 ? 1000 : f.code, f.reason, true);
          }
        }
      };
      conn.listen({
        data: (d) => {
          try {
            if (state.open) return onFrames(d);
            for (const ev of parser.push(d)) {
              if (ev.type === 'upgrade') {
                const accept = headerValue(ev.head.headers, 'sec-websocket-accept');
                if (accept !== wsAccept(key)) throw new Error('bad websocket accept');
                state.open = true;
                this.toPage({ t: 'ws.opened', id: m.id, protocol: headerValue(ev.head.headers, 'sec-websocket-protocol') ?? '' });
                if (ev.rest.length) onFrames(ev.rest);
              } else if (ev.type === 'data' && state.open) onFrames(ev.data);
              else if (ev.type === 'head') throw new Error(`upgrade refused (${ev.head.status})`);
            }
          } catch {
            gone(1006, '');
          }
        },
        end: () => gone(1006, ''),
        close: () => gone(1006, ''),
      });
      await conn.write(writeRequest({ method: 'GET', target, headers }));
    } catch {
      gone(1006, '');
    }
  }

  /** `wsGone` for socket `s` only: a later socket with the same id is left alone. */
  private wsGoneIf(s: WsState, id: number, code: number, reason: string, clean = false): void {
    if (this.sockets.get(id) === s) this.wsGone(id, code, reason, clean);
    else this.drop(s);
  }

  /** Close a socket's stream and give back its slot, once. */
  private drop(s: WsState): void {
    if (s.closed) return;
    s.closed = true;
    s.conn?.close();
    s.release();
  }

  private wsGone(id: number, code: number, reason: string, clean = false): void {
    const s = this.sockets.get(id);
    if (!s || s.closed) return;
    s.closed = true;
    this.sockets.delete(id);
    s.conn?.close();
    s.release();
    this.toPage({ t: 'ws.closed', id, code, reason, clean });
  }
}

const portIds = new WeakMap<MessagePort, number>();
let nextPort = 1;
/** Fetch ids are per worker channel; key them by channel too. */
function fetchKey(port: MessagePort, id: number): string {
  let n = portIds.get(port);
  if (!n) portIds.set(port, (n = nextPort++));
  return `${n}:${id}`;
}

/** Streams a response body to the worker, through a decompressor when the server compressed it. */
class BodyOut {
  private writer: WritableStreamDefaultWriter<Uint8Array> | null = null;
  private pump: Promise<void> | null = null;

  constructor(
    private readonly id: number,
    private readonly sink: Sink,
    decode: 'gzip' | 'deflate' | null,
  ) {
    if (decode && typeof DecompressionStream !== 'undefined') {
      const ds = new DecompressionStream(decode);
      this.writer = ds.writable.getWriter() as WritableStreamDefaultWriter<Uint8Array>;
      const reader = ds.readable.getReader();
      this.pump = (async () => {
        for (;;) {
          const { done, value } = await reader.read();
          if (done) return;
          this.send(value as Uint8Array);
        }
      })().catch(() => this.sink.post({ t: 'fetch.error', id: this.id, message: 'bad compressed body' }));
    }
  }

  private send(bytes: Uint8Array): void {
    const buf = bytes.slice().buffer as ArrayBuffer;
    this.sink.post({ t: 'fetch.data', id: this.id, data: buf }, [buf]);
  }

  write(bytes: Uint8Array): void {
    if (this.writer) void this.writer.write(bytes.slice()).catch(() => {});
    else this.send(bytes);
  }

  async end(): Promise<void> {
    if (this.writer) {
      await this.writer.close().catch(() => {});
      await this.pump;
    }
    this.sink.post({ t: 'fetch.end', id: this.id });
  }
}
