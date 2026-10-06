// Connection banner (spec 16 §9.1 Shell): amber after ~4 s without a host, red after ~15 s with
// Retry, a green flash on recovery. Driven by host states plus a clock tick; pure for tests.

import type { HostState } from '@vibeke/core';

export const AMBER_AFTER_MS = 4_000;
export const RED_AFTER_MS = 15_000;
export const GREEN_FOR_MS = 2_000;

export type BannerLevel = 'none' | 'amber' | 'red' | 'green';

export interface BannerState {
  level: BannerLevel;
  /** Hosts currently down (names). */
  down: string[];
  /** Down hosts that are revoked/incompatible (no point retrying). */
  fatal: string[];
}

// An expired share is not an outage; Settings shows it (and offers Forget).
const isDown = (h: HostState) => h.status !== 'online' && h.status !== 'idle' && h.status !== 'expired';
const isFatal = (h: HostState) => h.status === 'revoked' || h.status === 'incompatible';

export class BannerTracker {
  private downSince = new Map<string, number>();
  private shown: BannerLevel = 'none';
  private greenUntil = 0;

  update(hosts: readonly HostState[], now: number): BannerState {
    const present = new Set<string>();
    let oldest = Infinity;
    const down: string[] = [];
    const fatal: string[] = [];
    for (const h of hosts) {
      const id = h.record.host_id;
      present.add(id);
      if (isDown(h)) {
        if (!this.downSince.has(id)) this.downSince.set(id, now);
        oldest = Math.min(oldest, this.downSince.get(id)!);
        const name = h.info?.host_name ?? h.record.name;
        down.push(name);
        if (isFatal(h)) fatal.push(name);
      } else {
        this.downSince.delete(id);
      }
    }
    for (const id of [...this.downSince.keys()]) if (!present.has(id)) this.downSince.delete(id);

    let level: BannerLevel = 'none';
    if (down.length) {
      const age = now - oldest;
      if (fatal.length || age >= RED_AFTER_MS) level = 'red';
      else if (age >= AMBER_AFTER_MS) level = 'amber';
    }
    if (level === 'none' && (this.shown === 'amber' || this.shown === 'red')) this.greenUntil = now + GREEN_FOR_MS;
    if (level === 'none' && now < this.greenUntil) level = 'green';
    this.shown = level === 'green' ? 'green' : level;
    return { level, down, fatal };
  }
}
