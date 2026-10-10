// The "new version is waiting" state, set by the service worker registration (main.tsx) and read
// by the UI through `UiPlatform.appUpdate`. The page reloads only when the user taps.

export function createAppUpdate(applyUpdate: () => void) {
  let waiting = false;
  const listeners = new Set<() => void>();
  return {
    get: () => waiting,
    subscribe(cb: () => void) {
      listeners.add(cb);
      return () => listeners.delete(cb);
    },
    apply: () => applyUpdate(),
    /** Called when a new service worker is installed and waiting. */
    markWaiting() {
      if (waiting) return;
      waiting = true;
      for (const cb of [...listeners]) cb();
    },
  };
}

export type AppUpdate = ReturnType<typeof createAppUpdate>;
