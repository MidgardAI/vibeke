// Hash routes (spec 16 §9.3): `#/inbox`, workspaces `#/w/<host>/<workspace>[/t/<pane>]` with
// `?panel=changes|files|off&file=…&commit=…&base=…&view=diff&show=term|conversation|preview:<id>`, push deep links from the gateway
// (`#/i/<host>/<interaction>`, `#/r/<host>/<run>`, `#/inbox`, `#/approve/<host>[/<request>]`) and the
// pairing link `#/pair?d=…`.
// Older links (`#/h/<host>/p/<pane>[/history|/changes]`, `#/panes`, `#/focus`, `#/changes`) still
// parse; the app redirects them to a workspace once it knows the dashboard (app/selection.ts).

import { useSyncExternalStore } from 'react';

export type Tab = 'inbox' | 'panes' | 'focus' | 'changes';
export type PaneView = 'term' | 'history' | 'changes';
export type PanelKind = 'changes' | 'files';

export interface WorkspaceRoute {
  name: 'workspace';
  host: string;
  workspace: string;
  /** The selected tab (a pane id); null = the remembered or primary one. */
  pane: string | null;
  /** Right panel: explicit tab, `off`, or null = the per-device default. */
  panel: PanelKind | 'off' | null;
  file: string | null;
  commit: string | null;
  /** Changes compared against this ref (`?base=`) instead of the uncommitted work. */
  base?: string | null;
  /** `diff`: the centre shows `file`'s diff (from `commit` / `base` when set) as a transient view. */
  view?: 'diff' | null;
  /**
   * The tab's centre: null = the pane's default (agents: the workspace's agent view, see
   * lib/agent-view.ts), `term` / `conversation` (an agent's terminal or conversation), or
   * `preview:<id>`.
   */
  show?: string | null;
}

export type Route =
  | { name: 'home' }
  | { name: Tab }
  | { name: 'crew' }
  /** Incoming handoffs: every host's list, one host's, or one handoff's accept view. */
  | { name: 'handoffs'; host: string | null; id: string | null }
  /** Approval requests from panes: every host's, one host's, or one request's review. */
  | { name: 'approve'; host: string | null; id: string | null }
  | { name: 'settings'; section?: string }
  | { name: 'pair'; d: string | null }
  | WorkspaceRoute
  | { name: 'pane'; host: string; pane: string; view: PaneView; show?: string | null }
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
  const opt = (k: string) => {
    const v = query.get(k);
    return v ? v : null;
  };
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
    case 'handoffs':
      return { name: 'handoffs', host: b || null, id: (b && c) || null };
    case 'approve':
      return { name: 'approve', host: b || null, id: (b && c) || null };
    case 'settings':
      return b ? { name: 'settings', section: b } : { name: 'settings' };
    case 'pair': {
      // Keep the raw `d` value: URLSearchParams would turn base64url '-'/'_' safely, but '+' never
      // appears in base64url, so either is fine; read raw to avoid any decoding surprises.
      const m = /(?:^|[?&])d=([^&]*)/.exec(h.slice(q + 1));
      return { name: 'pair', d: q >= 0 && m ? m[1]! : null };
    }
    case 'w':
      if (b && c && (d === undefined || (d === 't' && e))) {
        const p = query.get('panel');
        return {
          name: 'workspace',
          host: b,
          workspace: c,
          pane: d === 't' ? e! : null,
          panel: p === 'changes' || p === 'files' || p === 'off' ? p : null,
          file: opt('file'),
          commit: opt('commit'),
          base: opt('base'),
          view: query.get('view') === 'diff' ? 'diff' : null,
          show: opt('show'),
        };
      }
      break;
    case 'h':
      if (b && c === 'p' && d) {
        const view: PaneView = e === 'history' || e === 'changes' ? e : 'term';
        const show = opt('show');
        return show ? { name: 'pane', host: b, pane: d, view, show } : { name: 'pane', host: b, pane: d, view };
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

/** A workspace route with defaults for the optional parts. */
export function workspaceRoute(host: string, workspace: string, o: Partial<Omit<WorkspaceRoute, 'name' | 'host' | 'workspace'>> = {}): WorkspaceRoute {
  return { name: 'workspace', host, workspace, pane: o.pane ?? null, panel: o.panel ?? null, file: o.file ?? null, commit: o.commit ?? null, base: o.base ?? null, view: o.view ?? null, show: o.show ?? null };
}

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
    case 'handoffs':
      return r.host ? `#/handoffs/${enc(r.host)}${r.id ? `/${enc(r.id)}` : ''}` : '#/handoffs';
    case 'approve':
      return r.host ? `#/approve/${enc(r.host)}${r.id ? `/${enc(r.id)}` : ''}` : '#/approve';
    case 'pair':
      return r.d ? `#/pair?d=${r.d}` : '#/pair';
    case 'workspace': {
      const q = new URLSearchParams();
      if (r.panel) q.set('panel', r.panel);
      if (r.file) q.set('file', r.file);
      if (r.commit) q.set('commit', r.commit);
      if (r.base) q.set('base', r.base);
      if (r.view) q.set('view', r.view);
      if (r.show) q.set('show', r.show);
      const qs = q.toString();
      return `#/w/${enc(r.host)}/${enc(r.workspace)}${r.pane ? `/t/${enc(r.pane)}` : ''}${qs ? `?${qs}` : ''}`;
    }
    case 'pane':
      return `#/h/${enc(r.host)}/p/${enc(r.pane)}${r.view === 'term' ? '' : `/${r.view}`}${r.show ? `?show=${enc(r.show)}` : ''}`;
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
    // pushState (not `location.hash =`) so every entry carries the marker `ensureParentEntry` checks.
    window.history.pushState({ vkNav: 1 }, '', hash);
    window.dispatchEvent(new HashChangeEvent('hashchange'));
  }
}

/** Routes that are one level below the inbox: Back from them goes up to it. */
export const isDeepRoute = (r: Route): boolean => r.name === 'workspace' || r.name === 'interaction' || r.name === 'run' || r.name === 'pane' || (r.name === 'approve' && !!r.id) || (r.name === 'handoffs' && !!r.id);

/**
 * After a cold start at a deep link (a notification, a shared link), put `parent` below it in the
 * history so Back goes up one level instead of leaving the app. Entries made by this app carry a
 * marker, so a reload or a normal visit adds nothing.
 */
export function ensureParentEntry(parent: Route): void {
  if (!hasWindow) return;
  try {
    if ((window.history.state as { vkNav?: number } | null)?.vkNav) return;
    const here = window.location.href;
    window.history.replaceState({ vkNav: 1 }, '', formatRoute(parent));
    window.history.pushState({ vkNav: 1 }, '', here);
  } catch {
    // history is locked down (sandboxed frame): Back just leaves
  }
}

export function goBack(fallback: Route = { name: 'home' }): void {
  if (hasWindow && window.history.length > 1) window.history.back();
  else navigate(fallback, { replace: true });
}
