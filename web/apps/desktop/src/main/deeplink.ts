// Deep links (spec 16 §16.2): `vibeke://…` URLs from the OS (open-url on macOS, argv on
// Windows/Linux via the single-instance lock) and pasted `https://<app>/#/pair?d=…` links, mapped
// to the app's hash routes. Anything unrecognised is ignored, never navigated to.

export const PROTOCOL = 'vibeke';

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
    case 'pair': {
      const dv = query.get('d');
      if (parts.length !== 1 || !dv || !B64URL.test(dv)) return null;
      return `#/pair?d=${dv}`;
    }
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
    case 'settings':
      return parts.length === 1 ? `#/${a}` : null;
    case 'i':
      if (parts.length === 3 && ID.test(b!) && ID.test(c!)) return `#/i/${enc(b!)}/${enc(c!)}`;
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
