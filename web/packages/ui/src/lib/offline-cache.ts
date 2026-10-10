// Offline cold start. The app saves each host's last dashboard (without open interactions) in the
// shell's storage. When the app starts and a host has not answered yet, the sidebar shows the
// saved rows dimmed with "as of <time>" instead of an empty list.

import type { Dashboard, HostState } from '@vibeke/core';

export interface CachedDashboard {
  /** When the dashboard was saved (the app's clock, ms). */
  at: number;
  dashboard: Dashboard;
}

/** A host as the UI sees it: `cachedAt` is set when its dashboard comes from the saved copy. */
export type HostView = HostState & { cachedAt?: number };

/** Minimum gap between saves of one host's dashboard. */
export const SAVE_EVERY_MS = 5000;

/**
 * What is worth saving: no open interactions (they cannot be answered offline and would mislead)
 * and no previews (their URLs point at the host's network).
 */
export function compactDashboard(d: Dashboard): Dashboard {
  const { previews: _previews, ...rest } = d;
  return { ...rest, interactions: [] };
}

/** Hosts without a live dashboard get their saved one, marked with `cachedAt`. */
export function overlayHosts(hosts: readonly HostState[], cached: Readonly<Record<string, CachedDashboard>>): readonly HostView[] {
  if (!Object.keys(cached).length) return hosts;
  let changed = false;
  const out = hosts.map((h): HostView => {
    const c = cached[h.record.host_id];
    if (h.dashboard || !c) return h;
    changed = true;
    return { ...h, dashboard: c.dashboard, cachedAt: c.at };
  });
  return changed ? out : hosts;
}

/** Is the host's data possibly old, and since when? `asOf` is null when unknown. */
export function staleInfo(h: HostView): { stale: boolean; asOf: number | null } {
  if (h.status === 'online') return { stale: false, asOf: null };
  if (!h.dashboard) return { stale: false, asOf: null };
  return { stale: true, asOf: h.cachedAt ?? h.lastOnlineAt ?? null };
}

/** Save a dashboard now? Once per `SAVE_EVERY_MS`, and only while the host is online and live. */
export function shouldSave(h: HostView, lastSaveMs: number | undefined, nowMs: number): boolean {
  if (h.status !== 'online' || !h.dashboard || h.cachedAt !== undefined) return false;
  return lastSaveMs === undefined || nowMs - lastSaveMs >= SAVE_EVERY_MS;
}
