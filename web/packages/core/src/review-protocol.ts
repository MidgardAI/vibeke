// Messages between the review controller (app origin) and the content origin (spec 20 §3.6).
//
//   bridge  → controller  window   {t:"ready"}
//   controller → bridge   window   {t:"port"} + [controller port]
//   bridge  → controller  port     {t:"channel"} + [service-worker port]   (also after a worker restart)
//   worker  → controller  channel  fetch, fetch.abort
//   controller → worker   channel  fetch.head, fetch.data, fetch.end, fetch.error
//   inject  → controller  window   {t:"cookies.get"} + [page port]             (once per page)
//   inject  → controller  port     ws.open, ws.send, ws.close, pick, cookies.get, cookies.set
//   controller → inject   port     cookies, annotate, reload, ws.opened, ws.message, ws.closed
//
// Everything from the content origin is untrusted: the preview's own code runs there. The
// controller accepts only the message kinds below, checked field by field (`parse*`); anything
// else is dropped. A message can at most ask for a request to the one preview the review device
// may reach anyway.
//
// No DOM types: `ArrayBuffer` and plain objects only, so the content bundle can share this file.

/** Every message carries this marker so unrelated `postMessage` traffic is ignored. */
export const REVIEW_PROTO = 'vk-review/1';

const MAX_URL = 8192;
const MAX_HEADERS = 128;
const MAX_HEADER_NAME = 256;
const MAX_HEADER_VALUE = 16 * 1024;
/** Largest request body the controller forwards (uploads in a dev app). */
export const MAX_REQUEST_BODY = 32 * 1024 * 1024;
/** Largest WebSocket message the controller forwards from the page. */
export const MAX_WS_SEND = 4 * 1024 * 1024;
const MAX_ID = 2 ** 31 - 1;
const METHODS = new Set(['GET', 'HEAD', 'POST', 'PUT', 'PATCH', 'DELETE', 'OPTIONS']);

export type Headers2 = [string, string][];

export interface FetchMsg {
  t: 'fetch';
  id: number;
  method: string;
  url: string;
  headers: Headers2;
  body: ArrayBuffer | null;
  /** A navigation (document) request: the HTML gets the injected script. */
  nav: boolean;
}
export interface FetchAbortMsg {
  t: 'fetch.abort';
  id: number;
}
export interface WsOpenMsg {
  t: 'ws.open';
  id: number;
  url: string;
  protocols: string[];
}
export interface WsSendMsg {
  t: 'ws.send';
  id: number;
  data: string | ArrayBuffer;
}
export interface WsCloseMsg {
  t: 'ws.close';
  id: number;
  code?: number;
  reason?: string;
}
export interface PickRect {
  x: number;
  y: number;
  w: number;
  h: number;
}
export interface PickPayload {
  kind: 'element' | 'region';
  selector: string;
  role?: string;
  name?: string;
  text?: string;
  html?: string;
  rect: PickRect;
  covers?: { selector: string; text?: string }[];
  url: string;
  viewport: { w: number; h: number; dpr?: number };
}
export interface PickMsg {
  t: 'pick';
  pick: PickPayload;
}
export interface CookiesGetMsg {
  t: 'cookies.get';
  /** The page's path (`location.pathname`). */
  path: string;
}
export interface CookiesSetMsg {
  t: 'cookies.set';
  cookie: string;
  path: string;
}

/** What the controller accepts from the service worker's channel. */
export type WorkerMessage = FetchMsg | FetchAbortMsg;
/** What the controller accepts from a page's port. */
export type PageMessage = WsOpenMsg | WsSendMsg | WsCloseMsg | PickMsg | CookiesGetMsg | CookiesSetMsg;
/** What the controller accepts from the bridge (window `ready`, port `channel`). */
export type BridgeMessage = { t: 'ready' } | { t: 'channel' };

// ---- controller → content (built by the controller, read by the content bundle) -----------

export type ToWorker =
  | { t: 'fetch.head'; id: number; status: number; statusText: string; headers: Headers2; cookies?: string }
  | { t: 'fetch.data'; id: number; data: ArrayBuffer }
  | { t: 'fetch.end'; id: number }
  | { t: 'fetch.error'; id: number; message: string };

export type ToPage =
  | { t: 'cookies'; visible: string }
  | { t: 'annotate'; on: boolean }
  /** Reload the page where it is (the frame is cross-origin to the controller). */
  | { t: 'reload' }
  | { t: 'ws.opened'; id: number; protocol: string }
  | { t: 'ws.message'; id: number; data: string | ArrayBuffer }
  | { t: 'ws.closed'; id: number; code: number; reason: string; clean: boolean };

/** Wrap a message with the protocol marker. */
export const wrap = <T extends object>(m: T): T & { p: string } => ({ p: REVIEW_PROTO, ...m });

// ---- validation -------------------------------------------------------------------------------

type Obj = Record<string, unknown>;

const isObj = (v: unknown): v is Obj => typeof v === 'object' && v !== null && !Array.isArray(v);
/** Exactly these keys (plus the marker), each optional key may be absent. */
function keysOk(o: Obj, required: string[], optional: string[] = []): boolean {
  if (o.p !== REVIEW_PROTO) return false;
  for (const k of required) if (!(k in o)) return false;
  for (const k of Object.keys(o)) if (k !== 'p' && k !== 't' && !required.includes(k) && !optional.includes(k)) return false;
  return true;
}
const isId = (v: unknown): v is number => typeof v === 'number' && Number.isInteger(v) && v > 0 && v <= MAX_ID;
const isStr = (v: unknown, max: number): v is string => typeof v === 'string' && v.length <= max;
const isNum = (v: unknown): v is number => typeof v === 'number' && Number.isFinite(v) && Math.abs(v) < 1e7;
const isBuf = (v: unknown): v is ArrayBuffer => Object.prototype.toString.call(v) === '[object ArrayBuffer]';

/** `url` is on the content origin and not one of the bundle's own `/__vk/` files. */
export function sameOriginUrl(url: string, contentOrigin: string): boolean {
  if (!url.startsWith(`${contentOrigin}/`)) return false;
  const path = url.slice(contentOrigin.length);
  return !path.startsWith('/__vk/');
}

function headersOk(v: unknown): v is Headers2 {
  if (!Array.isArray(v) || v.length > MAX_HEADERS) return false;
  for (const h of v) {
    if (!Array.isArray(h) || h.length !== 2 || !isStr(h[0], MAX_HEADER_NAME) || !isStr(h[1], MAX_HEADER_VALUE)) return false;
    if (!/^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(h[0]) || /[\r\n\0]/.test(h[1])) return false;
  }
  return true;
}

/** A message from the bridge window (`ready`) or the bridge port (`channel`); null otherwise. */
export function parseBridgeMessage(raw: unknown): BridgeMessage | null {
  if (!isObj(raw) || !keysOk(raw, [])) return null;
  if (raw.t === 'ready') return { t: 'ready' };
  if (raw.t === 'channel') return { t: 'channel' };
  return null;
}

/** A message on a service-worker channel; null when anything is off. */
export function parseWorkerMessage(raw: unknown, contentOrigin: string): WorkerMessage | null {
  if (!isObj(raw)) return null;
  switch (raw.t) {
    case 'fetch': {
      if (!keysOk(raw, ['id', 'method', 'url', 'headers', 'body', 'nav'])) return null;
      const { id, method, url, headers, body, nav } = raw;
      if (!isId(id) || typeof method !== 'string' || !METHODS.has(method)) return null;
      if (!isStr(url, MAX_URL) || !sameOriginUrl(url, contentOrigin)) return null;
      if (!headersOk(headers) || typeof nav !== 'boolean') return null;
      let buf: ArrayBuffer | null = null;
      if (body !== null) {
        if (!isBuf(body) || body.byteLength > MAX_REQUEST_BODY) return null;
        buf = body;
      }
      if ((method === 'GET' || method === 'HEAD') && buf && buf.byteLength > 0) return null;
      return { t: 'fetch', id, method, url, headers: headers.map(([k, v]) => [k, v] as [string, string]), body: buf, nav };
    }
    case 'fetch.abort':
      if (!keysOk(raw, ['id']) || !isId(raw.id)) return null;
      return { t: 'fetch.abort', id: raw.id };
  }
  return null;
}

/** The content origin's host, for checking WebSocket URLs (`ws(s)://<host>/…`). */
const hostOf = (origin: string): string => origin.replace(/^https?:\/\//, '');

function pickOk(v: unknown, contentOrigin: string): PickPayload | null {
  if (!isObj(v)) return null;
  const allowed = new Set(['kind', 'selector', 'role', 'name', 'text', 'html', 'rect', 'covers', 'url', 'viewport']);
  for (const k of Object.keys(v)) if (!allowed.has(k)) return null;
  if (v.kind !== 'element' && v.kind !== 'region') return null;
  if (!isStr(v.selector, 2048) || !v.selector) return null;
  for (const [k, max] of [['role', 64], ['name', 256], ['text', 2000], ['html', 2048]] as const) {
    if (v[k] !== undefined && !isStr(v[k], max)) return null;
  }
  const r = v.rect;
  if (!isObj(r) || !isNum(r.x) || !isNum(r.y) || !isNum(r.w) || !isNum(r.h) || r.w < 0 || r.h < 0) return null;
  if (!isStr(v.url, MAX_URL) || !(v.url === contentOrigin || sameOriginUrl(v.url, contentOrigin))) return null;
  const vp = v.viewport;
  if (!isObj(vp) || !isNum(vp.w) || !isNum(vp.h) || vp.w <= 0 || vp.h <= 0 || (vp.dpr !== undefined && !(isNum(vp.dpr) && vp.dpr > 0 && vp.dpr <= 8))) return null;
  let covers: { selector: string; text?: string }[] | undefined;
  if (v.covers !== undefined) {
    if (!Array.isArray(v.covers) || v.covers.length > 10) return null;
    covers = [];
    for (const c of v.covers) {
      if (!isObj(c) || !isStr(c.selector, 2048) || (c.text !== undefined && !isStr(c.text, 120))) return null;
      covers.push(c.text !== undefined ? { selector: c.selector, text: c.text } : { selector: c.selector });
    }
  }
  const out: PickPayload = {
    kind: v.kind,
    selector: v.selector,
    rect: { x: r.x, y: r.y, w: r.w, h: r.h },
    url: v.url,
    viewport: vp.dpr !== undefined ? { w: vp.w, h: vp.h, dpr: vp.dpr as number } : { w: vp.w, h: vp.h },
  };
  if (typeof v.role === 'string') out.role = v.role;
  if (typeof v.name === 'string') out.name = v.name;
  if (typeof v.text === 'string') out.text = v.text;
  if (typeof v.html === 'string') out.html = v.html;
  if (covers) out.covers = covers;
  return out;
}

/** A message on a page port (or the page's first window message); null when anything is off. */
export function parsePageMessage(raw: unknown, contentOrigin: string): PageMessage | null {
  if (!isObj(raw)) return null;
  switch (raw.t) {
    case 'ws.open': {
      if (!keysOk(raw, ['id', 'url', 'protocols']) || !isId(raw.id) || !isStr(raw.url, MAX_URL)) return null;
      const host = hostOf(contentOrigin);
      if (!raw.url.startsWith(`wss://${host}/`) && !raw.url.startsWith(`ws://${host}/`)) return null;
      if (!Array.isArray(raw.protocols) || raw.protocols.length > 16 || !raw.protocols.every((p) => isStr(p, 128) && /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/.test(p))) return null;
      return { t: 'ws.open', id: raw.id, url: raw.url, protocols: [...(raw.protocols as string[])] };
    }
    case 'ws.send': {
      if (!keysOk(raw, ['id', 'data']) || !isId(raw.id)) return null;
      if (typeof raw.data === 'string') return raw.data.length <= MAX_WS_SEND ? { t: 'ws.send', id: raw.id, data: raw.data } : null;
      if (isBuf(raw.data) && raw.data.byteLength <= MAX_WS_SEND) return { t: 'ws.send', id: raw.id, data: raw.data };
      return null;
    }
    case 'ws.close': {
      if (!keysOk(raw, ['id'], ['code', 'reason']) || !isId(raw.id)) return null;
      const out: WsCloseMsg = { t: 'ws.close', id: raw.id };
      if (raw.code !== undefined) {
        if (typeof raw.code !== 'number' || !(raw.code === 1000 || (raw.code >= 3000 && raw.code <= 4999))) return null;
        out.code = raw.code;
      }
      if (raw.reason !== undefined) {
        if (!isStr(raw.reason, 123)) return null;
        out.reason = raw.reason;
      }
      return out;
    }
    case 'pick': {
      if (!keysOk(raw, ['pick'])) return null;
      const pick = pickOk(raw.pick, contentOrigin);
      return pick ? { t: 'pick', pick } : null;
    }
    case 'cookies.get':
      if (!keysOk(raw, ['path']) || !isStr(raw.path, MAX_URL) || !raw.path.startsWith('/')) return null;
      return { t: 'cookies.get', path: raw.path };
    case 'cookies.set':
      if (!keysOk(raw, ['cookie', 'path']) || !isStr(raw.cookie, 8192) || !isStr(raw.path, MAX_URL) || !raw.path.startsWith('/')) return null;
      return { t: 'cookies.set', cookie: raw.cookie, path: raw.path };
  }
  return null;
}

// ---- the content origin -----------------------------------------------------------------------

/**
 * The content origin of a review link (`https://<sid>.p.<registrable domain of the app>`).
 * `pattern` overrides it, with `{sid}` replaced (local development, self-hosting):
 * `http://{sid}.p.localhost:5174`. Returns null when no origin can be derived (an IP address or a
 * single-label host without a pattern).
 */
export function reviewContentOrigin(appOrigin: string, sid: string, pattern?: string | null): string | null {
  const s = sid.toLowerCase();
  if (!/^[a-z2-7]{16}$/.test(s)) return null;
  if (pattern) {
    if (!pattern.includes('{sid}')) return null;
    const o = pattern.replace('{sid}', s).replace(/\/+$/, '');
    return /^https?:\/\/[a-z0-9.-]+(:\d+)?$/.test(o) ? o : null;
  }
  let u: URL;
  try {
    u = new URL(appOrigin);
  } catch {
    return null;
  }
  if (u.protocol !== 'https:') return null;
  const host = u.hostname.toLowerCase();
  if (/^[\d.]+$/.test(host) || host.includes(':')) return null;
  const labels = host.split('.');
  if (labels.length < 2) return null;
  // The last two labels; a public-suffix list is not worth its size here (self-hosters on a
  // multi-label suffix such as `example.co.uk` set the pattern).
  const domain = labels.slice(-2).join('.');
  return `https://${s}.p.${domain}${u.port ? `:${u.port}` : ''}`;
}
