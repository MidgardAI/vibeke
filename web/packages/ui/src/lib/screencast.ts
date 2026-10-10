// Live browser preview: frame math and the small rules around the screencast poll loop. Pure, so
// the geometry (fit, tap to page coordinates) is tested without a DOM.

export interface Size {
  width: number;
  height: number;
}

/** The largest size with the frame's aspect ratio that fits the box (scaled up or down). */
export function fitFrame(frame: Size, box: Size): Size {
  if (!(frame.width > 0) || !(frame.height > 0) || !(box.width > 0) || !(box.height > 0)) return { width: 0, height: 0 };
  const k = Math.min(box.width / frame.width, box.height / frame.height);
  return { width: Math.floor(frame.width * k), height: Math.floor(frame.height * k) };
}

/**
 * A tap at (`x`, `y`) inside the drawn frame (CSS pixels from its top left, `drawn` wide and high)
 * as a point in the page's viewport. Null when the tap is outside the frame.
 */
export function tapToPage(tap: { x: number; y: number }, drawn: Size, viewport: Size): { x: number; y: number } | null {
  if (!(drawn.width > 0) || !(drawn.height > 0) || !(viewport.width > 0) || !(viewport.height > 0)) return null;
  if (tap.x < 0 || tap.y < 0 || tap.x > drawn.width || tap.y > drawn.height) return null;
  const x = Math.min(viewport.width - 1, Math.round((tap.x / drawn.width) * viewport.width));
  const y = Math.min(viewport.height - 1, Math.round((tap.y / drawn.height) * viewport.height));
  return { x: Math.max(0, x), y: Math.max(0, y) };
}

/** Frames per second while the page is visible: 4 to 8. */
export const FRAME_FPS = 6;
export const frameDelayMs = (fps: number = FRAME_FPS): number => Math.round(1000 / Math.min(8, Math.max(4, fps)));

/** An image source for a frame, or null when the poll carried no newer frame. */
export function frameSrc(f: { mime?: string; data_b64: string | null }): string | null {
  if (!f.data_b64) return null;
  const mime = f.mime === 'image/png' ? 'image/png' : 'image/jpeg';
  return `data:${mime};base64,${f.data_b64}`;
}

/** The next `after_seq`: the newest sequence number seen. */
export const nextSeq = (current: number, f: { seq: number | null }): number => (typeof f.seq === 'number' && f.seq > current ? f.seq : current);

/** A poll that fails like this means the host dropped the attachment (idle timeout): attach again. */
export const needsReattach = (kind: string): boolean => kind === 'conflict' || kind === 'not_found';

/** Who drives the session, from this device's point of view. */
export type Controller = 'you' | 'someone' | 'agent';
export const controllerOf = (humanControl: boolean, mine: boolean): Controller => (mine ? 'you' : humanControl ? 'someone' : 'agent');

/** What the address field sends: a missing scheme becomes `https://` (`http://` for local hosts). Null when empty. */
export function normalizeUrl(input: string): string | null {
  const s = input.trim();
  if (!s) return null;
  if (/^[a-z][a-z0-9+.-]*:\/\//i.test(s) || /^(about|data|file):/i.test(s)) return s;
  const local = /^(localhost|127\.|\[::1\]|0\.0\.0\.0)(:|\/|$)/i.test(s);
  return `${local ? 'http' : 'https'}://${s}`;
}

/** Keys the phone offers as buttons, with the name sent to `browser.press`. */
export const PRESS_KEYS: { key: string; label: string }[] = [
  { key: 'Enter', label: 'Enter' },
  { key: 'Tab', label: 'Tab' },
  { key: 'Escape', label: 'Esc' },
  { key: 'Backspace', label: 'Backspace' },
  { key: 'ArrowUp', label: '↑' },
  { key: 'ArrowDown', label: '↓' },
  { key: 'ArrowLeft', label: '←' },
  { key: 'ArrowRight', label: '→' },
];

/** Sessions on the same site as `url` first (same origin), the others after, each group in order. */
export function sessionsFor<T extends { url: string }>(sessions: readonly T[], url: string): T[] {
  const origin = (u: string): string | null => {
    try {
      return new URL(u).origin;
    } catch {
      return null;
    }
  };
  const want = origin(url);
  const near = sessions.filter((s) => want !== null && origin(s.url) === want);
  return [...near, ...sessions.filter((s) => !near.includes(s))];
}
