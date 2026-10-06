// Path resolution for the app:// handler (Electron-free, unit tested).

import { join, normalize, sep } from 'node:path';

/** Resolve a request path inside `root`, or null when it escapes. */
export function resolveAppPath(root: string, pathname: string): string | null {
  let p: string;
  try {
    p = decodeURIComponent(pathname);
  } catch {
    return null;
  }
  if (p.includes('\0')) return null;
  if (p === '/' || p === '') p = '/index.html';
  const full = normalize(join(root, p));
  const base = normalize(root.endsWith(sep) ? root : root + sep);
  return full.startsWith(base) ? full : null;
}
