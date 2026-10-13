// The script the service worker adds to every HTML page of the preview (spec 20 §3.6). It runs
// before the page's own scripts and:
//
// - replaces `document.cookie` with the non-HttpOnly cookies of the controller's jar (the jar
//   itself lives on the app origin; HttpOnly cookies never come here);
// - replaces `window.WebSocket` for this origin with a shim that runs the socket through the
//   controller (service workers cannot catch WebSockets), so hot reload works;
// - contains the annotate overlay: while the controller has it on, a click picks an element and a
//   drag picks an area, and the pick goes to the controller.
//
// It talks to the controller over a MessagePort sent with its first message (`cookies.get`). Only
// the top preview frame (whose parent is the controller) does that; nested frames keep the native
// WebSocket.

import { REVIEW_PROTO, type ToPage } from '@vibeke/core/review-protocol';
import { pickElement, pickRegion } from 'virtual:vk-pick';

(() => {
  const w = window as Window & { __vkInjected?: boolean };
  if (w.__vkInjected) return;
  w.__vkInjected = true;

  const script = document.currentScript as HTMLScriptElement | null;
  let cookies = script?.dataset.vkCookies ?? '';
  const NativeWebSocket = window.WebSocket;
  const isTop = window.parent !== window && window.parent === window.top;
  let port: MessagePort | null = null;

  const post = (m: Record<string, unknown>, transfer: Transferable[] = []) => {
    try {
      port?.postMessage({ p: REVIEW_PROTO, ...m }, transfer);
    } catch {
      // the controller went away
    }
  };

  // ---- document.cookie ------------------------------------------------------------------------

  /** Apply a `document.cookie = "…"` write to the local copy until the controller answers. */
  const applyLocal = (text: string) => {
    const first = text.split(';')[0] ?? '';
    const eq = first.indexOf('=');
    if (eq <= 0) return;
    const name = first.slice(0, eq).trim();
    const value = first.slice(eq + 1).trim();
    const gone = /;\s*max-age\s*=\s*(-\d+|0)\b/i.test(text) || (() => {
      const m = /;\s*expires\s*=\s*([^;]+)/i.exec(text);
      return !!m && Date.parse(m[1]!) <= Date.now();
    })();
    const list = cookies ? cookies.split('; ').filter((c) => c.split('=')[0] !== name) : [];
    if (!gone) list.push(`${name}=${value}`);
    cookies = list.join('; ');
  };

  try {
    Object.defineProperty(document, 'cookie', {
      configurable: true,
      get: () => cookies,
      set: (v: unknown) => {
        const text = String(v);
        applyLocal(text);
        post({ t: 'cookies.set', cookie: text.slice(0, 8192), path: location.pathname });
      },
    });
  } catch {
    // a browser that refuses the override keeps its own (empty) cookies
  }

  // ---- WebSocket shim ---------------------------------------------------------------------------

  const sockets = new Map<number, ShimSocket>();
  let nextSocket = 1;

  class ShimSocket extends EventTarget {
    static readonly CONNECTING = 0;
    static readonly OPEN = 1;
    static readonly CLOSING = 2;
    static readonly CLOSED = 3;
    readonly CONNECTING = 0;
    readonly OPEN = 1;
    readonly CLOSING = 2;
    readonly CLOSED = 3;
    readonly url: string;
    readonly id = nextSocket++;
    readyState = 0;
    protocol = '';
    extensions = '';
    bufferedAmount = 0;
    binaryType: BinaryType = 'blob';
    onopen: ((e: Event) => void) | null = null;
    onmessage: ((e: MessageEvent) => void) | null = null;
    onclose: ((e: CloseEvent) => void) | null = null;
    onerror: ((e: Event) => void) | null = null;
    private sendChain: Promise<void> = Promise.resolve();

    constructor(url: string, protocols: string[]) {
      super();
      this.url = url;
      sockets.set(this.id, this);
      post({ t: 'ws.open', id: this.id, url, protocols });
    }

    private fire(e: Event, handler: ((e: never) => void) | null): void {
      this.dispatchEvent(e);
      try {
        handler?.(e as never);
      } catch (err) {
        setTimeout(() => {
          throw err;
        });
      }
    }

    opened(protocol: string): void {
      if (this.readyState !== 0) return;
      this.readyState = 1;
      this.protocol = protocol;
      this.fire(new Event('open'), this.onopen);
    }

    message(data: string | ArrayBuffer): void {
      if (this.readyState !== 1) return;
      const payload = typeof data === 'string' || this.binaryType === 'arraybuffer' ? data : new Blob([data]);
      this.fire(new MessageEvent('message', { data: payload, origin: location.origin }), this.onmessage);
    }

    closed(code: number, reason: string, clean: boolean): void {
      if (this.readyState === 3) return;
      const wasOpen = this.readyState === 1 || this.readyState === 2;
      this.readyState = 3;
      sockets.delete(this.id);
      if (!clean || !wasOpen) this.fire(new Event('error'), this.onerror);
      this.fire(new CloseEvent('close', { code, reason, wasClean: clean }), this.onclose);
    }

    send(data: string | ArrayBuffer | ArrayBufferView | Blob): void {
      if (this.readyState === 0) throw new DOMException('WebSocket is not open', 'InvalidStateError');
      if (this.readyState !== 1) return;
      this.sendChain = this.sendChain.then(async () => {
        if (typeof data === 'string') return post({ t: 'ws.send', id: this.id, data });
        let buf: ArrayBuffer;
        if (data instanceof Blob) buf = await data.arrayBuffer();
        else if (data instanceof ArrayBuffer) buf = data.slice(0);
        else buf = new Uint8Array(data.buffer, data.byteOffset, data.byteLength).slice().buffer as ArrayBuffer;
        post({ t: 'ws.send', id: this.id, data: buf }, [buf]);
      });
    }

    close(code?: number, reason?: string): void {
      if (this.readyState >= 2) return;
      this.readyState = 2;
      const m: Record<string, unknown> = { t: 'ws.close', id: this.id };
      if (code !== undefined) m.code = code;
      if (reason !== undefined) m.reason = String(reason).slice(0, 123);
      post(m);
    }
  }

  /** `new WebSocket(url, protocols)`: this origin goes through the controller, others are native. */
  function VkWebSocket(this: unknown, url: string | URL, protocols?: string | string[]): WebSocket {
    const abs = new URL(String(url), location.href);
    if (abs.protocol === 'http:') abs.protocol = 'ws:';
    if (abs.protocol === 'https:') abs.protocol = 'wss:';
    if (!port || abs.host !== location.host) return new NativeWebSocket(url, protocols);
    const list = protocols === undefined ? [] : Array.isArray(protocols) ? protocols : [protocols];
    return new ShimSocket(abs.href, list.map(String)) as unknown as WebSocket;
  }
  VkWebSocket.prototype = NativeWebSocket.prototype;
  Object.assign(VkWebSocket, { CONNECTING: 0, OPEN: 1, CLOSING: 2, CLOSED: 3 });

  // ---- annotate overlay -----------------------------------------------------------------------

  let annotating = false;
  let host: HTMLElement | null = null;
  let box: HTMLElement | null = null;
  let down: { x: number; y: number; id: number } | null = null;
  let dragging = false;
  const DRAG_PX = 8;

  const ensureOverlay = () => {
    if (host) return;
    host = document.createElement('vk-annotate');
    host.style.cssText = 'position:fixed;inset:0;pointer-events:none;z-index:2147483647;';
    const root = host.attachShadow({ mode: 'closed' });
    box = document.createElement('div');
    box.style.cssText = 'position:fixed;display:none;border:2px solid #3b82f6;background:rgba(59,130,246,.12);border-radius:3px;pointer-events:none;box-sizing:border-box;';
    root.appendChild(box);
    document.documentElement.appendChild(host);
  };

  const show = (x: number, y: number, wd: number, ht: number) => {
    if (!box) return;
    box.style.display = 'block';
    box.style.left = `${x}px`;
    box.style.top = `${y}px`;
    box.style.width = `${wd}px`;
    box.style.height = `${ht}px`;
  };
  const hide = () => {
    if (box) box.style.display = 'none';
  };

  const viewport = () => ({ w: window.innerWidth, h: window.innerHeight, dpr: Math.min(8, Math.max(0.1, window.devicePixelRatio || 1)) });
  const clip = (s: unknown, max: number) => String(s ?? '').slice(0, max);

  const sendPick = (pick: Record<string, unknown>) => post({ t: 'pick', pick: { ...pick, url: location.href, viewport: viewport() } });

  const stop = (e: Event) => {
    e.preventDefault();
    e.stopPropagation();
    e.stopImmediatePropagation();
  };

  const onDown = (e: PointerEvent) => {
    if (!annotating || !e.isPrimary) return;
    stop(e);
    down = { x: e.clientX, y: e.clientY, id: e.pointerId };
    dragging = false;
  };
  const onMove = (e: PointerEvent) => {
    if (!annotating) return;
    if (down && e.pointerId === down.id) {
      stop(e);
      if (Math.abs(e.clientX - down.x) >= DRAG_PX || Math.abs(e.clientY - down.y) >= DRAG_PX) dragging = true;
      if (dragging) show(Math.min(down.x, e.clientX), Math.min(down.y, e.clientY), Math.abs(e.clientX - down.x), Math.abs(e.clientY - down.y));
      return;
    }
    const el = document.elementFromPoint(e.clientX, e.clientY);
    if (!el || el === host) return hide();
    const r = el.getBoundingClientRect();
    show(r.x, r.y, r.width, r.height);
  };
  const onUp = (e: PointerEvent) => {
    if (!annotating || !down || e.pointerId !== down.id) return;
    stop(e);
    const start = down;
    down = null;
    hide();
    if (dragging) {
      const rect = { x: Math.round(Math.min(start.x, e.clientX)), y: Math.round(Math.min(start.y, e.clientY)), w: Math.round(Math.abs(e.clientX - start.x)), h: Math.round(Math.abs(e.clientY - start.y)) };
      try {
        const r = pickRegion(rect.x, rect.y, rect.w, rect.h);
        sendPick({ kind: 'region', selector: clip(r.container, 2048) || 'body', rect, covers: r.covers.slice(0, 10).map((c) => ({ selector: clip(c.selector, 2048), ...(c.text !== undefined ? { text: clip(c.text, 120) } : {}) })) });
      } catch {
        // the page changed under the pick
      }
      return;
    }
    const el = document.elementFromPoint(e.clientX, e.clientY);
    if (!el) return;
    try {
      const p = pickElement.call(el);
      if (!p || !p.selector) return;
      sendPick({ kind: 'element', selector: clip(p.selector, 2048), role: clip(p.role, 64), name: clip(p.name, 256), text: clip(p.text, 2000), html: clip(p.html, 2048), rect: p.rect });
    } catch {
      // ignore
    }
  };
  /** Clicks never reach the page while annotating. */
  const swallow = (e: Event) => {
    if (annotating) stop(e);
  };

  const setAnnotate = (on: boolean) => {
    annotating = on;
    if (on) ensureOverlay();
    else {
      hide();
      down = null;
    }
    document.documentElement.style.cursor = on ? 'crosshair' : '';
  };

  for (const [type, f] of [
    ['pointerdown', onDown],
    ['pointermove', onMove],
    ['pointerup', onUp],
  ] as const) {
    window.addEventListener(type, f as EventListener, { capture: true });
  }
  for (const type of ['click', 'dblclick', 'auxclick', 'contextmenu', 'mousedown', 'mouseup', 'touchstart', 'touchend', 'submit']) {
    window.addEventListener(type, swallow, { capture: true, passive: false });
  }

  // ---- the controller ---------------------------------------------------------------------------

  const onMessage = (e: MessageEvent) => {
    const m = e.data as (ToPage & { p?: unknown }) | null;
    if (!m || m.p !== REVIEW_PROTO) return;
    switch (m.t) {
      case 'cookies':
        if (typeof m.visible === 'string') cookies = m.visible;
        break;
      case 'annotate':
        setAnnotate(!!m.on);
        break;
      case 'reload':
        location.reload();
        break;
      case 'ws.opened':
        sockets.get(m.id)?.opened(String(m.protocol ?? ''));
        break;
      case 'ws.message':
        sockets.get(m.id)?.message(m.data);
        break;
      case 'ws.closed':
        sockets.get(m.id)?.closed(m.code, m.reason, m.clean);
        break;
    }
  };

  if (isTop) {
    const ch = new MessageChannel();
    port = ch.port1;
    port.onmessage = onMessage;
    port.start();
    const target = (location as Location & { ancestorOrigins?: DOMStringList }).ancestorOrigins?.[0] ?? '*';
    window.parent.postMessage({ p: REVIEW_PROTO, t: 'cookies.get', path: location.pathname }, target, [ch.port2]);
    (window as unknown as { WebSocket: unknown }).WebSocket = VkWebSocket;
  }
})();
