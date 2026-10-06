// The bundled renderer is served from `app://vibeke/…` (a privileged, secure, standard scheme):
// a stable origin for localStorage and the IPC sender check, a strict CSP header, and no
// file:// URLs. Path traversal outside the renderer directory is refused.

import { readFile } from 'node:fs/promises';
import { extname } from 'node:path';
import { protocol } from 'electron';
import { resolveAppPath } from './app-path';

export const APP_SCHEME = 'app';
export const APP_ORIGIN = `${APP_SCHEME}://vibeke`;

/** Must run before `app.whenReady()`. */
export function registerScheme(): void {
  protocol.registerSchemesAsPrivileged([
    { scheme: APP_SCHEME, privileges: { standard: true, secure: true, supportFetchAPI: true, codeCache: true } },
  ]);
}

export const CSP = [
  "default-src 'self'",
  "script-src 'self'",
  // React style props and Tailwind's runtime-free output; no inline scripts.
  "style-src 'self' 'unsafe-inline'",
  "img-src 'self' data: blob:",
  "font-src 'self' data:",
  "media-src 'self' blob:",
  // Everything network-bound goes through the main process (IPC), never from the page.
  "connect-src 'self'",
  "object-src 'none'",
  "base-uri 'none'",
  "form-action 'none'",
  "frame-src 'none'",
  "frame-ancestors 'none'",
  "worker-src 'self' blob:",
].join('; ');

const TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.map': 'application/json',
};

export function handleAppProtocol(root: string): void {
  protocol.handle(APP_SCHEME, async (req) => {
    const url = new URL(req.url);
    if (url.host !== 'vibeke') return new Response('not found', { status: 404 });
    const file = resolveAppPath(root, url.pathname);
    if (!file) return new Response('forbidden', { status: 403 });
    try {
      const body = await readFile(file);
      const type = TYPES[extname(file)] ?? 'application/octet-stream';
      const headers: Record<string, string> = {
        'content-type': type,
        'x-content-type-options': 'nosniff',
        'cache-control': 'no-cache',
      };
      if (type.startsWith('text/html')) {
        headers['content-security-policy'] = CSP;
        headers['referrer-policy'] = 'no-referrer';
      }
      return new Response(body, { status: 200, headers });
    } catch {
      return new Response('not found', { status: 404 });
    }
  });
}
