// Hash routes (spec 16 §9.3): `#/overview`, `#/inbox`, workspaces `#/w/<host>/<workspace>[/t/<pane>]` with
// `?panel=changes|files|screenshots|previews|off&file=…&commit=…&base=…&view=diff&show=term|conversation&preview=<id>&expand=1`, push deep links from the gateway
// (`#/i/<host>/<interaction>`, `#/r/<host>/<run>`, `#/inbox`, `#/approve/<host>[/<request>]`) and the
// pairing link `#/pair?d=…`, review and continue links (`#/review?d=…`, `#/continue?d=…`), the
// GitHub sign-in return `#/attest?t=…`, and shared content `#/share-in/<id>`.
// Older links (`#/h/<host>/p/<pane>[/history|/changes]`, `#/panes`, `#/focus`, `#/changes`) still
// parse; the app redirects them to a workspace once it knows the dashboard (app/selection.ts).

import { useSyncExternalStore } from 'react';

export type Tab = 'inbox' | 'panes' | 'focus' | 'changes';
export type PaneView = 'term' | 'history' | 'changes';
export type PanelKind = 'changes' | 'files' | 'screenshots' | 'previews';

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
   * lib/agent-view.ts), or `term` / `conversation` (an agent's terminal or conversation). The
   * older `preview:<id>` becomes `preview` with the Previews panel.
   */
  show?: string | null;
  /** The preview shown in the Previews panel (`?preview=`), or in the centre when `expanded`. */
  preview?: string | null;
  /** `?expand=1`: the selected preview fills the centre instead of the panel (spec 06 B3.5). */
  expanded?: boolean;
}

export type Route =
  | { name: 'home' }
  /** Every host's workspaces at a glance: counts, requests, working agents, a feed. */
  | { name: 'overview' }
  | { name: 'tui'; host: string; workspace?: string; pane?: string }
  | { name: Tab }
  | { name: 'crew' }
  /** Cloud sandboxes of the own hosts (spec 17). */
  | { name: 'sandboxes' }
  /** Incoming handoffs: every host's list, one host's, or one handoff's accept view. */
  | { name: 'handoffs'; host: string | null; id: string | null }
  /** Approval requests from panes: every host's, one host's, or one request's review. */
  | { name: 'approve'; host: string | null; id: string | null }
  | { name: 'settings'; section?: string }
  | { name: 'pair'; d: string | null }
  /** A design-review link (spec 20 §3): pairs, then shows the preview under review. */
  | { name: 'review'; d: string | null }
  /** The account server's return from a GitHub sign-in (spec 20 §2.3): `?t=` or `?error=`. */
  | { name: 'attest'; query: string }
  /** A continue link (spec 20 §4): pick one of the own hosts to take the session. */
  | { name: 'continue'; d: string | null }
  /** Content shared into the app (Web Share Target), under a one-time id. */
  | { name: 'share_in'; id: string }
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
    case 'overview':
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
      return { name: a };
    case 'crew':
      return { name: 'crew' };
    case 'sandboxes':
      return { name: 'sandboxes' };
    case 'tui':
      if (b && !c) return { name: 'tui', host: b, ...(opt('workspace') ? { workspace: opt('workspace')! } : {}), ...(opt('pane') ? { pane: opt('pane')! } : {}) };
      break;
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
    case 'review':
    case 'continue': {
      const m = /(?:^|[?&])d=([^&]*)/.exec(h.slice(q + 1));
      return { name: a, d: q >= 0 && m ? m[1]! : null };
    }
    case 'attest':
      return { name: 'attest', query: q >= 0 ? h.slice(q + 1) : '' };
    case 'share-in':
      if (b) return { name: 'share_in', id: b };
      break;
    case 'w':
      if (b && c && (d === undefined || (d === 't' && e))) {
        const p = query.get('panel');
        return workspaceRoute(b, c, {
          pane: d === 't' ? e! : null,
          panel: p === 'changes' || p === 'files' || p === 'screenshots' || p === 'previews' || p === 'off' ? p : null,
          file: opt('file'),
          commit: opt('commit'),
          base: opt('base'),
          view: query.get('view') === 'diff' ? 'diff' : null,
          show: opt('show'),
          preview: opt('preview'),
          expanded: query.get('expand') === '1',
        });
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

/**
 * A workspace route with defaults for the optional parts. An older `show=preview:<id>` (preview
 * tabs) opens that preview in the Previews panel instead.
 */
export function workspaceRoute(host: string, workspace: string, o: Partial<Omit<WorkspaceRoute, 'name' | 'host' | 'workspace'>> = {}): WorkspaceRoute {
  const legacy = o.show?.startsWith('preview:') ? o.show.slice('preview:'.length) || null : null;
  const preview = o.preview ?? legacy;
  // An old preview link always shows its preview, whatever panel it named.
  const panel = legacy && !o.expanded ? 'previews' : (o.panel ?? null);
  return {
    name: 'workspace',
    host,
    workspace,
    pane: o.pane ?? null,
    panel,
    file: o.file ?? null,
    commit: o.commit ?? null,
    base: o.base ?? null,
    view: o.view ?? null,
    show: o.show?.startsWith('preview:') ? null : (o.show ?? null),
    preview: preview ?? null,
    expanded: !!preview && !!o.expanded,
  };
}

export function formatRoute(r: Route): string {
  switch (r.name) {
    case 'home':
      return '#/';
    case 'tui':
      return `#/tui/${enc(r.host)}` + (r.workspace || r.pane ? '?' + new URLSearchParams({ ...(r.workspace ? { workspace: r.workspace } : {}), ...(r.pane ? { pane: r.pane } : {}) }) : '');
    case 'overview':
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
    case 'crew':
    case 'sandboxes':
      return `#/${r.name}`;
    case 'settings':
      return r.section ? `#/settings/${enc(r.section)}` : '#/settings';
    case 'handoffs':
      return r.host ? `#/handoffs/${enc(r.host)}${r.id ? `/${enc(r.id)}` : ''}` : '#/handoffs';
    case 'approve':
      return r.host ? `#/approve/${enc(r.host)}${r.id ? `/${enc(r.id)}` : ''}` : '#/approve';
    case 'share_in':
      return `#/share-in/${enc(r.id)}`;
    case 'pair':
    case 'review':
    case 'continue':
      return r.d ? `#/${r.name}?d=${r.d}` : `#/${r.name}`;
    case 'attest':
      return r.query ? `#/attest?${r.query}` : '#/attest';
    case 'workspace': {
      const q = new URLSearchParams();
      if (r.panel) q.set('panel', r.panel);
      if (r.file) q.set('file', r.file);
      if (r.commit) q.set('commit', r.commit);
      if (r.base) q.set('base', r.base);
      if (r.view) q.set('view', r.view);
      if (r.show) q.set('show', r.show);
      if (r.preview) q.set('preview', r.preview);
      if (r.preview && r.expanded) q.set('expand', '1');
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

/** Routes that are one level below the overview: Back from them goes up to it. */
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
