// A registry of reasons the app must not reload right now (unsent composer text, an upload in
// flight, an open sheet). The update prompt waits while any reason is registered and says why.
// Components call `useReloadGuard(active, reason)`; the registry has no DOM or React dependency.

import { useEffect, useSyncExternalStore } from 'react';

export type ReloadReason = 'draft' | 'upload' | 'sheet';

const counts = new Map<ReloadReason, number>();
const listeners = new Set<() => void>();
let snapshot: readonly ReloadReason[] = [];

function publish(): void {
  snapshot = [...counts.keys()].sort();
  for (const cb of [...listeners]) cb();
}

/** Register a busy reason. Returns a release function (safe to call twice). */
export function holdReload(reason: ReloadReason): () => void {
  counts.set(reason, (counts.get(reason) ?? 0) + 1);
  publish();
  let released = false;
  return () => {
    if (released) return;
    released = true;
    const n = (counts.get(reason) ?? 1) - 1;
    if (n <= 0) counts.delete(reason);
    else counts.set(reason, n);
    publish();
  };
}

/** The reasons that currently block a reload, in a stable order. */
export const reloadBlockers = (): readonly ReloadReason[] => snapshot;

export function subscribeReloadGuard(cb: () => void): () => void {
  listeners.add(cb);
  return () => listeners.delete(cb);
}

/** Hold a reload reason while `active` is true. */
export function useReloadGuard(active: boolean, reason: ReloadReason): void {
  useEffect(() => (active ? holdReload(reason) : undefined), [active, reason]);
}

export function useReloadBlockers(): readonly ReloadReason[] {
  return useSyncExternalStore(subscribeReloadGuard, reloadBlockers, reloadBlockers);
}

/** Test helper: forget every registration. */
export function resetReloadGuard(): void {
  counts.clear();
  publish();
}
