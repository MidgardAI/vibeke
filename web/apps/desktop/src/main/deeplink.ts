// Deep links (spec 16 §16.2): `vibeke://…` URLs from the OS (open-url on macOS, argv on
// Windows/Linux via the single-instance lock) and pasted `https://<app>/#/pair?d=…` links, mapped
// to the app's hash routes. Anything unrecognised is ignored, never navigated to.

import { needsAttestation, parseLink } from '@vibeke/core';

export const PROTOCOL = 'vibeke';
/** The browser app that opens links this app cannot (spec 20 §7.3). */
export const BROWSER_APP_URL = 'https://app.vibeke.dev';

const B64URL = /^[A-Za-z0-9_-]{16,8000}$/;
const ID = /^[A-Za-z0-9_.:-]{1,200}$/;

/**
 * `vibeke://pair?d=X` / `vibeke:pair?d=X` / `https://host/#/pair?d=X` → `#/pair?d=X`
 * `vibeke://i/<host>/<id>` → `#/i/<host>/<id>`, `vibeke://inbox` → `#/inbox`,
 * `vibeke://h/<host>/p/<pane>` → `#/h/<host>/p/<pane>`. Returns null for anything else.
 */
export function deepLinkToHash(raw: string): string | null {
  const s = raw.trim();
  if (s.length > 9000) return null;
  let u: URL;
  try {
    u = new URL(s);
  } catch {
    return null;
  }
  let path: string;
  let query: URLSearchParams;
  if (u.protocol === `${PROTOCOL}:`) {
    // `vibeke://pair?d=…` parses with host "pair"; `vibeke:pair?d=…` / `vibeke:///pair` with a path.
    path = `${u.host}${u.pathname}`.replace(/^\/+/, '');
    query = u.searchParams;
    // Also accept `vibeke://open#/pair?d=…`.
    if (u.hash.startsWith('#/')) return fromHash(u.hash);
  } else if (u.protocol === 'https:' || u.protocol === 'http:') {
    return fromHash(u.hash);
  } else return null;
  return fromParts(path.replace(/\/+$/, ''), query);
}

function fromHash(hash: string): string | null {
  if (!hash.startsWith('#/')) return null;
  const h = hash.slice(2);
  const q = h.indexOf('?');
  const path = q >= 0 ? h.slice(0, q) : h;
  return fromParts(path, new URLSearchParams(q >= 0 ? h.slice(q + 1) : ''));
}

function fromParts(path: string, query: URLSearchParams): string | null {
  const parts = path.split('/').filter(Boolean);
  const [a, b, c, d] = parts;
  switch (a) {
    case 'pair':
    case 'review':
    case 'continue': {
      const dv = query.get('d');
      if (parts.length !== 1 || !dv || !B64URL.test(dv)) return null;
      return `#/${a}?d=${dv}`;
    }
    case 'overview':
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
    case 'settings':
      return parts.length === 1 ? `#/${a}` : null;
    case 'i':
      if (parts.length === 3 && ID.test(b!) && ID.test(c!)) return `#/i/${enc(b!)}/${enc(c!)}`;
      return null;
    case 'w':
      if (parts.length === 3 && ID.test(b!) && ID.test(c!)) return `#/w/${enc(b!)}/${enc(c!)}`;
      if (parts.length === 5 && ID.test(b!) && ID.test(c!) && d === 't' && ID.test(parts[4]!)) return `#/w/${enc(b!)}/${enc(c!)}/t/${enc(parts[4]!)}`;
      return null;
    case 'h':
      if (parts.length === 4 && ID.test(b!) && c === 'p' && ID.test(d!)) return `#/h/${enc(b!)}/p/${enc(d!)}`;
      return null;
    case undefined:
      return '#/';
    default:
      return null;
  }
}

const enc = encodeURIComponent;

/** The deep link among process arguments (Windows/Linux pass it as an argv entry). */
export function deepLinkFromArgv(argv: readonly string[]): string | null {
  for (const a of argv) if (a.toLowerCase().startsWith(`${PROTOCOL}:`)) return a;
  return null;
}

export type DeepLinkAction = { kind: 'route'; hash: string } | { kind: 'external'; url: string };

/**
 * What to do with a deep link (spec 20 §7.3): most map to a route in this app; review links and
 * GitHub-gated pair links open in the system browser, because the GitHub sign-in round trip and the
 * review screen live in the browser app. `https://…` links open as they are; `vibeke://` forms open
 * on `browserApp`.
 */
export function deepLinkAction(raw: string, browserApp = BROWSER_APP_URL): DeepLinkAction | null {
  const hash = deepLinkToHash(raw);
  if (!hash) return null;
  const m = /^#\/(pair|review)\?d=(.*)$/.exec(hash);
  if (m) {
    let external = m[1] === 'review';
    if (!external) {
      try {
        external = needsAttestation(parseLink(m[2]!));
      } catch {
        external = false;
      }
    }
    if (external) {
      const s = raw.trim();
      const url = /^https?:\/\//i.test(s) ? s : `${browserApp.replace(/\/+$/, '')}/${hash}`;
      return { kind: 'external', url };
    }
  }
  return { kind: 'route', hash };
}
