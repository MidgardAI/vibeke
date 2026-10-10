// Scroll positions of lists the user leaves and comes back to (the sidebar list, the inbox).
// Kept in memory for the life of the window; restored before paint.

import { useLayoutEffect, type RefObject } from 'react';

const positions = new Map<string, number>();

export const getScroll = (key: string): number => positions.get(key) ?? 0;
export const saveScroll = (key: string, top: number): void => {
  if (top > 0) positions.set(key, Math.round(top));
  else positions.delete(key);
};

/** Restore `key`'s position when the element mounts (or the key changes) and record changes. */
export function useScrollMemory(key: string | undefined, ref: RefObject<HTMLElement | null>): void {
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || !key) return;
    const want = getScroll(key);
    // The element can be reused by another screen: start from the saved spot, or from the top.
    el.scrollTop = want;
    // Content that renders a frame later: try once more when the first try was clamped.
    if (want > 0 && Math.abs(el.scrollTop - want) > 1) requestAnimationFrame(() => (el.scrollTop = want));
    // Saved on every scroll event (a Map write): the element may be detached before cleanup runs.
    const onScroll = () => saveScroll(key, el.scrollTop);
    el.addEventListener('scroll', onScroll, { passive: true });
    return () => el.removeEventListener('scroll', onScroll);
  }, [key, ref]);
}
