// Open overlays (dialogs, the command palette, popovers, menus). Native layers above the page,
// such as the desktop's embedded preview view, hide while any is open so the overlay stays
// visible and clickable (spec 06 B3.5).

import { createContext, useContext, useLayoutEffect, useSyncExternalStore } from 'react';

let open = 0;
const listeners = new Set<() => void>();

function emit(): void {
  for (const cb of [...listeners]) cb();
}

/** Registers one open overlay; call the result when it closes (idempotent). */
export function pushOverlay(): () => void {
  open++;
  emit();
  let done = false;
  return () => {
    if (done) return;
    done = true;
    open = Math.max(0, open - 1);
    emit();
  };
}

/** Number of overlays open now. */
export const overlayCount = (): number => open;

export function subscribeOverlays(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** Counts the calling component as an open overlay while `active` (default: while mounted). */
export function useOverlay(active = true): void {
  useLayoutEffect(() => (active ? pushOverlay() : undefined), [active]);
}

/**
 * Overlays that hold the subtree itself (the phone-sized panel sheet with a preview in it). They
 * do not cover it, so they do not count for `useOverlayOpen` below.
 */
export const OverlayBase = createContext(0);

/** True while an overlay is open above the calling component. */
export function useOverlayOpen(): boolean {
  const base = useContext(OverlayBase);
  return useSyncExternalStore(
    subscribeOverlays,
    () => open > base,
    () => false,
  );
}
