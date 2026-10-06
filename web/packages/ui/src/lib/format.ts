import { t } from '../i18n';

/** Compact duration: `now`, `42s`, `5m`, `3h`, `2d`. */
export function shortDuration(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 5) return t.time.now;
  if (s < 60) return t.time.s(s);
  const m = Math.floor(s / 60);
  if (m < 60) return t.time.m(m);
  const h = Math.floor(m / 60);
  if (h < 48) return t.time.h(h);
  return t.time.d(Math.floor(h / 24));
}

export const ago = (thenMs: number, nowMs: number): string => {
  const d = shortDuration(nowMs - thenMs);
  return d === t.time.now ? d : t.time.ago(d);
};

export const clockTime = (ms: number): string =>
  new Date(ms).toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' });

/** Last path component(s), `~` for home-ish prefixes. */
export function shortPath(p: string | null | undefined, parts = 2): string {
  if (!p) return '';
  const clean = p.replace(/^\/(?:Users|home)\/[^/]+/, '~');
  const segs = clean.split('/').filter(Boolean);
  if (segs.length <= parts) return clean;
  return `…/${segs.slice(-parts).join('/')}`;
}

export const basename = (p: string | null | undefined): string => (p ? (p.split('/').filter(Boolean).pop() ?? p) : '');

export const capitalize = (s: string): string => (s ? s[0]!.toUpperCase() + s.slice(1) : s);

/** Standard (padded) base64, as the server's `image.upload` / `stt.transcribe` decode it. */
export function base64Std(bytes: Uint8Array): string {
  let bin = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    bin += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(bin);
}

/** `16:00` today, else `Tue 16:00` (within a week), else a date. */
export function whenText(ms: number, nowMs: number): string {
  const d = new Date(ms);
  const n = new Date(nowMs);
  if (d.toDateString() === n.toDateString()) return clockTime(ms);
  if (Math.abs(ms - nowMs) < 6 * 86_400_000) {
    return `${d.toLocaleDateString(undefined, { weekday: 'short' })} ${clockTime(ms)}`;
  }
  return d.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
}

/** `512 B`, `3.4 KiB`, `12.0 MiB`. */
export function byteSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  return `${(n / 1024 / 1024).toFixed(1)} MiB`;
}
