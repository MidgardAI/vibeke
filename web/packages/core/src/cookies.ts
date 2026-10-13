// The review controller's cookie jar (spec 20 §3.6). A service worker cannot install cookies
// from a response it builds, and anything stored on the content origin is visible to the
// preview's own script. So the jar lives in the controller (app origin): it stores `Set-Cookie`
// headers from responses, adds the `Cookie` header to requests, and hands the content origin only
// the non-HttpOnly cookies (for the `document.cookie` shim).
//
// There is one host (the preview), so `Domain` is ignored and every cookie is host-only.
// `SameSite` is ignored: everything is same-origin from the dev server's view. `Secure` is kept
// for display but not enforced (the tunnel to the dev server is plain HTTP by design).

export interface Cookie {
  name: string;
  value: string;
  /** Path attribute (default-path when absent). */
  path: string;
  /** Unix ms; null: a session cookie (kept until the jar is dropped). */
  expires: number | null;
  httpOnly: boolean;
  secure: boolean;
  sameSite?: string;
  /** Unix ms the cookie was first set (ordering for equal path lengths). */
  created: number;
}

/** Stored form of a jar (IndexedDB value, keyed by review `sid`). */
export interface CookieJarData {
  v: 1;
  cookies: Cookie[];
}

/** Persistence for jars, keyed by review `sid` (the PWA uses IndexedDB). */
export interface CookieJarStore {
  get(sid: string): Promise<CookieJarData | null>;
  set(sid: string, data: CookieJarData): Promise<void>;
  remove(sid: string): Promise<void>;
}

/** Most cookies kept per jar; the oldest go first. */
export const MAX_COOKIES = 300;
/** Longest `name=value` kept (browsers use 4096). */
export const MAX_COOKIE_BYTES = 4096;

/** The default-path of a request path (RFC 6265 §5.1.4). */
export function defaultPath(requestPath: string): string {
  const p = requestPath.split('?')[0]!.split('#')[0]!;
  if (!p.startsWith('/')) return '/';
  const i = p.lastIndexOf('/');
  return i <= 0 ? '/' : p.slice(0, i);
}

/** Does `requestPath` path-match `cookiePath` (RFC 6265 §5.1.4)? */
export function pathMatches(requestPath: string, cookiePath: string): boolean {
  const p = requestPath.split('?')[0]!.split('#')[0]! || '/';
  if (p === cookiePath) return true;
  if (!p.startsWith(cookiePath)) return false;
  return cookiePath.endsWith('/') || p[cookiePath.length] === '/';
}

const CTL = /[\x00-\x08\x0a-\x1f\x7f]/;
const TOKEN = /^[!#$%&'*+\-.^_`|~0-9A-Za-z]+$/;

/**
 * Parse one `Set-Cookie` header (RFC 6265 §5.2). Returns null for a header to ignore. A cookie
 * whose expiry is in the past is returned with `expires <= now` (the jar deletes it).
 */
export function parseSetCookie(header: string, nowMs: number, requestPath = '/'): Cookie | null {
  if (header.length > MAX_COOKIE_BYTES * 2 || CTL.test(header)) return null;
  const parts = header.split(';');
  const first = parts.shift()!;
  const eq = first.indexOf('=');
  if (eq < 0) return null;
  const name = first.slice(0, eq).trim();
  const value = first.slice(eq + 1).trim();
  if (!name || !TOKEN.test(name) || name.length + value.length > MAX_COOKIE_BYTES) return null;
  let path: string | null = null;
  let expires: number | null = null;
  let maxAge: number | null = null;
  let httpOnly = false;
  let secure = false;
  let sameSite: string | undefined;
  for (const raw of parts) {
    const i = raw.indexOf('=');
    const key = (i < 0 ? raw : raw.slice(0, i)).trim().toLowerCase();
    const val = i < 0 ? '' : raw.slice(i + 1).trim();
    switch (key) {
      case 'expires': {
        const t = Date.parse(val);
        if (Number.isFinite(t)) expires = t;
        break;
      }
      case 'max-age':
        if (/^-?\d+$/.test(val)) maxAge = Number(val);
        break;
      case 'path':
        path = val.startsWith('/') ? val : null;
        break;
      case 'httponly':
        httpOnly = true;
        break;
      case 'secure':
        secure = true;
        break;
      case 'samesite':
        sameSite = val.toLowerCase();
        break;
      // `domain`, `partitioned`, `priority` and unknown attributes: ignored (one host).
    }
  }
  // Max-Age wins over Expires (RFC 6265 §5.3 step 3).
  if (maxAge !== null) expires = maxAge <= 0 ? 0 : nowMs + maxAge * 1000;
  const c: Cookie = { name, value, path: path ?? defaultPath(requestPath), expires, httpOnly, secure, created: nowMs };
  if (sameSite) c.sameSite = sameSite;
  return c;
}

const live = (c: Cookie, nowMs: number): boolean => c.expires === null || c.expires > nowMs;

export class CookieJar {
  private cookies: Cookie[] = [];

  constructor(data?: CookieJarData | null) {
    if (data?.v === 1 && Array.isArray(data.cookies)) {
      for (const c of data.cookies) if (isCookie(c)) this.cookies.push({ ...c });
      this.cookies = this.cookies.slice(-MAX_COOKIES);
    }
  }

  /** Store one `Set-Cookie` header from a response to `requestPath`. True when the jar changed. */
  setFromHeader(header: string, nowMs: number, requestPath = '/'): boolean {
    const c = parseSetCookie(header, nowMs, requestPath);
    return c ? this.store(c, nowMs, true) : false;
  }

  /**
   * A `document.cookie = "…"` write from the page: same syntax, but it can neither create an
   * HttpOnly cookie nor replace one (RFC 6265 §5.3 step 10). True when the jar changed.
   */
  setFromDocument(text: string, nowMs: number, pagePath = '/'): boolean {
    const c = parseSetCookie(text, nowMs, pagePath);
    if (!c || c.httpOnly) return false;
    return this.store(c, nowMs, false);
  }

  private store(c: Cookie, nowMs: number, fromHttp: boolean): boolean {
    const i = this.cookies.findIndex((x) => x.name === c.name && x.path === c.path);
    const old = i >= 0 ? this.cookies[i]! : null;
    if (old && old.httpOnly && !fromHttp) return false;
    if (old) {
      c.created = old.created;
      this.cookies.splice(i, 1);
    }
    if (!live(c, nowMs)) return old !== null;
    this.cookies.push(c);
    this.prune(nowMs);
    return true;
  }

  private prune(nowMs: number): void {
    this.cookies = this.cookies.filter((c) => live(c, nowMs));
    if (this.cookies.length > MAX_COOKIES) this.cookies = this.cookies.slice(-MAX_COOKIES);
  }

  /** Cookies sent with a request for `requestPath`: longer paths first, then older first. */
  matching(requestPath: string, nowMs: number): Cookie[] {
    return this.cookies
      .filter((c) => live(c, nowMs) && pathMatches(requestPath, c.path))
      .sort((a, b) => b.path.length - a.path.length || a.created - b.created);
  }

  /** The `Cookie` request header for `requestPath`, or null when no cookie applies. */
  header(requestPath: string, nowMs: number): string | null {
    const list = this.matching(requestPath, nowMs);
    return list.length ? list.map((c) => `${c.name}=${c.value}`).join('; ') : null;
  }

  /** What `document.cookie` reads on `pagePath`: the non-HttpOnly cookies only. */
  visible(pagePath: string, nowMs: number): string {
    return this.matching(pagePath, nowMs)
      .filter((c) => !c.httpOnly)
      .map((c) => `${c.name}=${c.value}`)
      .join('; ');
  }

  /** Every live cookie (for persistence and tests). */
  all(nowMs: number): Cookie[] {
    return this.cookies.filter((c) => live(c, nowMs)).map((c) => ({ ...c }));
  }

  /**
   * The stored form. Session cookies are kept too: a review tab that reloads is still the same
   * browsing session from the reviewer's view.
   */
  toJSON(nowMs: number): CookieJarData {
    return { v: 1, cookies: this.all(nowMs) };
  }

  clear(): void {
    this.cookies = [];
  }
}

function isCookie(c: unknown): c is Cookie {
  const o = c as Cookie;
  return (
    typeof o === 'object' &&
    o !== null &&
    typeof o.name === 'string' &&
    typeof o.value === 'string' &&
    typeof o.path === 'string' &&
    (o.expires === null || typeof o.expires === 'number') &&
    typeof o.httpOnly === 'boolean' &&
    typeof o.secure === 'boolean' &&
    typeof o.created === 'number'
  );
}
