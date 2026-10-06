// Notification housekeeping (spec 16 §7.8): one notification per host, tagged `vibeke:<host id>`.
// Items resolve silently (no "clear" pushes), so on every foreground the app closes notifications
// of hosts with nothing open, and keeps the app badge equal to the open interaction count.

import type { HostState } from '@vibeke/core';

export const hostOfTag = (tag: string): string | null => {
  const m = /^vibeke:([^:]+)/.exec(tag);
  return m ? m[1]! : null;
};

/** Open interactions per host id (only hosts with a dashboard). */
export function openCounts(hosts: readonly HostState[]): Map<string, number> {
  const m = new Map<string, number>();
  for (const h of hosts) {
    if (!h.dashboard) continue;
    m.set(h.record.host_id, h.dashboard.interactions.filter((i) => i.status === 'open').length);
  }
  return m;
}

/**
 * Tags to close: a host we know about (dashboard loaded) with nothing open, or a host that is no
 * longer paired. Hosts whose state is unknown (offline, no dashboard yet) keep their notification.
 */
export function staleTags(tags: readonly string[], hosts: readonly HostState[]): string[] {
  const counts = openCounts(hosts);
  const paired = new Set(hosts.map((h) => h.record.host_id));
  return tags.filter((tag) => {
    const host = hostOfTag(tag);
    if (!host) return false;
    if (!paired.has(host)) return true;
    if (tag.endsWith(':test')) return true;
    const n = counts.get(host);
    return n === 0;
  });
}

export function badgeCount(hosts: readonly HostState[]): number {
  let n = 0;
  for (const c of openCounts(hosts).values()) n += c;
  return n;
}
