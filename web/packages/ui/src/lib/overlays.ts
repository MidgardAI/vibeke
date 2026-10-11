// Open overlays (dialogs, the command palette, popovers, menus). Native layers above the page,
// such as the desktop's embedded preview view, hide while any is open so the overlay stays
// visible and clickable (spec 06 B3.5).

import { useLayoutEffect, useSyncExternalStore } from 'react';

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

/** True while any overlay is open. */
export function useOverlayOpen(): boolean {
  return useSyncExternalStore(
    subscribeOverlays,
    () => open > 0,
    () => false,
  );
}
