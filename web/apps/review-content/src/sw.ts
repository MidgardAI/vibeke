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

let port: MessagePort | null = null;
let waiters: ((p: MessagePort | null) => void)[] = [];
let nextId = 1;

interface Pending {
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
  setPort(p);
});

function setPort(p: MessagePort): void {
  // Requests in flight on the old channel cannot finish any more.
  if (port && port !== p) {
    for (const [id, h] of pending) {
      h.error('the review reconnected');
      pending.delete(id);
    }
    port.close();
  }
  port = p;
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
      return h.end();
    case 'fetch.error':
      pending.delete(m.id);
      return h.error(typeof m.message === 'string' ? m.message : 'error');
  }
}

const isBridge = (url: string): boolean => {
  const p = new URL(url).pathname;
  return p === '/__vk/bridge' || p === '/__vk/bridge.html';
};

/** The channel to the controller: the current one, or a fresh one from the bridge (≤ 10 s). */
async function channel(): Promise<MessagePort | null> {
  if (port) return port;
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
