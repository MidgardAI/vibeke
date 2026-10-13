// The content origin's service worker (spec 20 §3.6). It serves every request of the preview
// frame through the review controller (app origin): each fetch goes over a MessageChannel the
// bridge relays, the controller sends it through the encrypted tunnel to the dev server, and the
// response comes back in pieces. It never handles `/__vk/*` (the static host serves those) and
// never stores anything: it holds no secrets.
//
// Browsers stop idle workers; a restarted worker has no channel. On its first fetch it asks the
// bridge page for one and waits at most 10 s, then answers with a "lost connection" page.

/// <reference lib="webworker" />
import { REVIEW_PROTO, type ToWorker } from '@vibeke/core/review-protocol';
import { injectStream, injectTag, lostPage } from './html';

declare const self: ServiceWorkerGlobalScope;

const CHANNEL_WAIT_MS = 10_000;
const HEAD_WAIT_MS = 60_000;
const NULL_BODY = new Set([101, 103, 204, 205, 304]);
/** Navigation responses may be framed only by the review controller's origins. */
const FRAME_ANCESTORS = `frame-ancestors ${__VK_APP_ORIGINS__.join(' ')}`;

/**
 * One channel per review tab, keyed by that tab's bridge client. Every tab of one review reaches
 * the same preview, so any live channel can serve a request; a tab that closes drops only its own.
 */
const ports = new Map<string, MessagePort>();
let waiters: ((p: MessagePort | null) => void)[] = [];
let nextId = 1;

interface Pending {
  /** The channel the request went out on. */
  port: MessagePort;
  head(m: Extract<ToWorker, { t: 'fetch.head' }>): void;
  data(b: ArrayBuffer): void;
  end(): void;
  error(message: string): void;
}
const pending = new Map<number, Pending>();

self.addEventListener('install', () => void self.skipWaiting());
self.addEventListener('activate', (e) => e.waitUntil(self.clients.claim()));

self.addEventListener('message', (e) => {
  const d = e.data as { p?: unknown; t?: unknown } | null;
  const p = e.ports[0];
  if (d?.p !== REVIEW_PROTO || d.t !== 'channel' || !p) return;
  const source = e.source as Client | null;
  setPort(source?.id ?? `anon-${nextId++}`, p);
});

/** Requests in flight on channel `p` cannot finish any more. */
function failOn(p: MessagePort, message: string): void {
  for (const [id, h] of pending) {
    if (h.port !== p) continue;
    h.error(message);
    pending.delete(id);
  }
}

const retired = new Set<MessagePort>();

/** Close a replaced channel when its last pending request is done. */
function retire(p: MessagePort): void {
  retired.add(p);
  closeIfIdle(p);
}

function closeIfIdle(p: MessagePort): void {
  if (!retired.has(p)) return;
  for (const h of pending.values()) if (h.port === p) return;
  retired.delete(p);
  p.close();
}

function setPort(client: string, p: MessagePort): void {
  // A newer channel from the same tab (the bridge may send one on its own and another when
  // asked) serves new requests; requests already sent on the older one finish there. Close the
  // older one once nothing is pending on it.
  const old = ports.get(client);
  if (old && old !== p) retire(old);
  ports.set(client, p);
  p.onmessage = (ev) => onPortMessage(ev.data);
  p.start();
  const w = waiters;
  waiters = [];
  for (const f of w) f(p);
}

function onPortMessage(raw: unknown): void {
  const m = raw as ToWorker & { p?: unknown };
  if (!m || m.p !== REVIEW_PROTO || typeof m.id !== 'number') return;
  const h = pending.get(m.id);
  if (!h) return;
  switch (m.t) {
    case 'fetch.head':
      return h.head(m);
    case 'fetch.data':
      return h.data(m.data);
    case 'fetch.end':
      pending.delete(m.id);
      h.end();
      return closeIfIdle(h.port);
    case 'fetch.error':
      pending.delete(m.id);
      h.error(typeof m.message === 'string' ? m.message : 'error');
      return closeIfIdle(h.port);
  }
}

const isBridge = (url: string): boolean => {
  const p = new URL(url).pathname;
  return p === '/__vk/bridge' || p === '/__vk/bridge.html';
};

/** A live channel to a controller: one whose bridge tab is still open, or a fresh one (≤ 10 s). */
async function channel(): Promise<MessagePort | null> {
  for (const [client, p] of [...ports]) {
    if (await self.clients.get(client)) return p;
    // That tab closed: its channel is dead.
    ports.delete(client);
    failOn(p, 'the review tab closed');
    p.close();
  }
  const wait = new Promise<MessagePort | null>((resolve) => {
    waiters.push(resolve);
    setTimeout(() => resolve(null), CHANNEL_WAIT_MS);
  });
  const clients = await self.clients.matchAll({ includeUncontrolled: true, type: 'window' });
  for (const c of clients) if (isBridge(c.url)) c.postMessage({ p: REVIEW_PROTO, t: 'need-channel' });
  return wait;
}

self.addEventListener('fetch', (e) => {
  const url = new URL(e.request.url);
  if (url.origin !== self.location.origin) return;
  if (url.pathname.startsWith('/__vk/')) return;
  e.respondWith(forward(e.request));
});

const lost = (status = 502): Response =>
  new Response(lostPage(), { status, headers: { 'Content-Type': 'text/html; charset=utf-8', 'Cache-Control': 'no-store' } });

async function forward(req: Request): Promise<Response> {
  const p = await channel();
  if (!p) return lost();
  const id = nextId++;
  const nav = req.mode === 'navigate' || req.destination === 'document' || req.destination === 'iframe';
  const body = req.method === 'GET' || req.method === 'HEAD' ? null : await req.arrayBuffer();
  const headers: [string, string][] = [];
  req.headers.forEach((v, k) => headers.push([k, v]));

  return new Promise<Response>((resolve) => {
    let controller: ReadableStreamDefaultController<Uint8Array> | null = null;
    let settled = false;
    // A timeout also cancels the request at the controller, so its tunnel slot is freed.
    const timer = setTimeout(() => {
      try {
        p.postMessage({ p: REVIEW_PROTO, t: 'fetch.abort', id });
      } catch {
        // the channel is gone
      }
      fail('the preview did not answer');
    }, HEAD_WAIT_MS);
    const fail = (message: string) => {
      pending.delete(id);
      if (!settled) {
        settled = true;
        clearTimeout(timer);
        resolve(new Response(`${message}\n`, { status: 502, headers: { 'Content-Type': 'text/plain; charset=utf-8' } }));
      } else controller?.error(new Error(message));
    };
    pending.set(id, {
      port: p,
      head(m) {
        if (settled) return;
        settled = true;
        clearTimeout(timer);
        const h = new Headers();
        for (const [k, v] of m.headers) {
          try {
            h.append(k, v);
          } catch {
            // a header the browser does not accept in a synthesized response
          }
        }
        if (nav) h.append('Content-Security-Policy', FRAME_ANCESTORS);
        const status = m.status >= 200 && m.status <= 599 ? m.status : 502;
        if (NULL_BODY.has(status) || req.method === 'HEAD') {
          resolve(new Response(null, { status, statusText: m.statusText, headers: h }));
          return;
        }
        let stream = new ReadableStream<Uint8Array>({
          start(c) {
            controller = c;
          },
          cancel() {
            pending.delete(id);
            p.postMessage({ p: REVIEW_PROTO, t: 'fetch.abort', id });
          },
        });
        const html = (h.get('content-type') ?? '').toLowerCase().includes('text/html');
        if (nav && html) stream = injectStream(stream, injectTag(m.cookies ?? ''));
        resolve(new Response(stream, { status, statusText: m.statusText, headers: h }));
      },
      data(b) {
        try {
          controller?.enqueue(new Uint8Array(b));
        } catch {
          // the page stopped reading
        }
      },
      end() {
        try {
          controller?.close();
        } catch {
          // already closed
        }
        if (!settled) fail('empty response');
      },
      error: fail,
    });
    req.signal?.addEventListener('abort', () => {
      if (!pending.has(id)) return;
      pending.delete(id);
      p.postMessage({ p: REVIEW_PROTO, t: 'fetch.abort', id });
    });
    try {
      p.postMessage({ p: REVIEW_PROTO, t: 'fetch', id, method: req.method, url: req.url, headers, body, nav }, body ? [body] : []);
    } catch (e) {
      fail((e as Error).message);
    }
  });
}
