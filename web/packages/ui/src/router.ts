// Hash routes (spec 16 §9.3): `#/inbox`, `#/h/<host>/p/<pane>`, push deep links from the gateway
// (`#/i/<host>/<interaction>`, `#/r/<host>/<run>`, `#/inbox`) and the pairing link `#/pair?d=…`.

import { useSyncExternalStore } from 'react';

export type Tab = 'inbox' | 'panes' | 'focus' | 'changes';
export type PaneView = 'term' | 'history' | 'changes';

export type Route =
  | { name: 'home' }
  | { name: Tab }
  | { name: 'crew' }
  | { name: 'settings'; section?: string }
  | { name: 'pair'; d: string | null }
  | { name: 'pane'; host: string; pane: string; view: PaneView }
  | { name: 'interaction'; host: string; id: string; preselect: 'allow' | 'deny' | null }
  | { name: 'run'; host: string; run: string }
  | { name: 'not_found'; path: string };

const dec = (s: string | undefined) => {
  try {
    return decodeURIComponent(s ?? '');
  } catch {
    return s ?? '';
  }
};

export function parseRoute(hash: string): Route {
  let h = hash.replace(/^#/, '');
  if (!h.startsWith('/')) h = `/${h}`;
  const q = h.indexOf('?');
  const path = q >= 0 ? h.slice(0, q) : h;
  const query = new URLSearchParams(q >= 0 ? h.slice(q + 1) : '');
  const parts = path.split('/').filter(Boolean).map(dec);
  const [a, b, c, d, e] = parts;
  switch (a) {
    case undefined:
      return { name: 'home' };
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
      return { name: a };
    case 'crew':
      return { name: 'crew' };
    case 'settings':
      return b ? { name: 'settings', section: b } : { name: 'settings' };
    case 'pair': {
      // Keep the raw `d` value: URLSearchParams would turn base64url '-'/'_' safely, but '+' never
      // appears in base64url, so either is fine; read raw to avoid any decoding surprises.
      const m = /(?:^|[?&])d=([^&]*)/.exec(h.slice(q + 1));
      return { name: 'pair', d: q >= 0 && m ? m[1]! : null };
    }
    case 'h':
      if (b && c === 'p' && d) {
        const view: PaneView = e === 'history' || e === 'changes' ? e : 'term';
        return { name: 'pane', host: b, pane: d, view };
      }
      break;
    case 'i':
      if (b && c) {
        const pre = query.get('do');
        return { name: 'interaction', host: b, id: c, preselect: pre === 'allow' || pre === 'deny' ? pre : null };
      }
      break;
    case 'r':
      if (b && c) return { name: 'run', host: b, run: c };
      break;
  }
  return { name: 'not_found', path };
}

const enc = encodeURIComponent;

export function formatRoute(r: Route): string {
  switch (r.name) {
    case 'home':
      return '#/';
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
    case 'crew':
      return `#/${r.name}`;
    case 'settings':
      return r.section ? `#/settings/${enc(r.section)}` : '#/settings';
    case 'pair':
      return r.d ? `#/pair?d=${r.d}` : '#/pair';
    case 'pane':
      return `#/h/${enc(r.host)}/p/${enc(r.pane)}${r.view === 'term' ? '' : `/${r.view}`}`;
    case 'interaction':
      return `#/i/${enc(r.host)}/${enc(r.id)}${r.preselect ? `?do=${r.preselect}` : ''}`;
    case 'run':
      return `#/r/${enc(r.host)}/${enc(r.run)}`;
    case 'not_found':
      return `#${r.path}`;
  }
}

/** Normalize a deep link from a push payload (`#/i/…`, `/#/i/…`, `#/`) to a hash. */
export function hashFromUrl(url: string): string {
  const i = url.indexOf('#');
  return i >= 0 ? url.slice(i) : '#/';
}

// ---- live hash store ----------------------------------------------------------------------

const hasWindow = typeof window !== 'undefined';
const subscribe = (cb: () => void) => {
  if (!hasWindow) return () => {};
  window.addEventListener('hashchange', cb);
  return () => window.removeEventListener('hashchange', cb);
};
const getHash = () => (hasWindow ? window.location.hash : '#/');

export function useRoute(): Route {
  const hash = useSyncExternalStore(subscribe, getHash, () => '#/');
  return parseRoute(hash);
}

export function navigate(to: Route | string, opts: { replace?: boolean } = {}): void {
  if (!hasWindow) return;
  const hash = typeof to === 'string' ? to : formatRoute(to);
  if (opts.replace) {
    window.history.replaceState(window.history.state, '', hash);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  } else if (window.location.hash !== hash) {
    window.location.hash = hash;
  }
}

export function goBack(fallback: Route = { name: 'home' }): void {
  if (hasWindow && window.history.length > 1) window.history.back();
  else navigate(fallback, { replace: true });
}
