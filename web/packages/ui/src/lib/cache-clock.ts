// Prompt-cache countdown. An agent's prompt cache expires a fixed time after its last turn, and a
// turn sent after that costs more and runs slower. The chip counts down from the moment the run
// went idle. The math is pure; `createSharedClock` is the one ticker all chips read.

import type { AgentRun } from '@vibeke/core';
import type { Readable } from './store';

/** Default time-to-live in minutes per harness. Unknown harnesses have no chip. */
export const DEFAULT_CACHE_TTL_MIN: Readonly<Record<string, number>> = { claude: 5, codex: 5 };
export const CACHE_TTL_MIN_RANGE = { min: 1, max: 120 } as const;
/** A cold chip stays visible this long after expiry, then the list gets quiet again. */
export const COLD_VISIBLE_MS = 60 * 60_000;
/** The last quarter of the time-to-live is the warning state. */
export const LOW_FRACTION = 0.25;

/** Time-to-live in ms for a harness: the user's override, else the default, else null (no chip). */
export function cacheTtlMs(harness: string, overrides: Readonly<Record<string, number>> = {}): number | null {
  const o = overrides[harness];
  const min = typeof o === 'number' && Number.isFinite(o) && o > 0 ? o : DEFAULT_CACHE_TTL_MIN[harness];
  return min === undefined ? null : Math.round(min * 60_000);
}

export type CacheState = 'warm' | 'low' | 'cold';

export interface CacheStatus {
  state: CacheState;
  /** Time left; 0 once cold. */
  remainingMs: number;
  /** Time since expiry; 0 while warm. */
  coldForMs: number;
  /** remaining / ttl, 0..1. */
  fraction: number;
}

/** Where the cache stands `nowMs` when the last turn ended at `sinceMs`. */
export function cacheStatus(sinceMs: number, nowMs: number, ttlMs: number): CacheStatus {
  const left = sinceMs + ttlMs - nowMs;
  if (left <= 0) return { state: 'cold', remainingMs: 0, coldForMs: -left, fraction: 0 };
  const fraction = Math.min(1, left / ttlMs);
  return { state: fraction <= LOW_FRACTION ? 'low' : 'warm', remainingMs: left, coldForMs: 0, fraction };
}

/** `<1m` under a minute, else whole minutes rounded up (`4m`, `1h 5m`). */
export function cacheLabel(remainingMs: number): string {
  if (remainingMs < 60_000) return '<1m';
  const m = Math.ceil(remainingMs / 60_000);
  if (m < 60) return `${m}m`;
  const h = Math.floor(m / 60);
  return m % 60 ? `${h}h ${m % 60}m` : `${h}h`;
}

/**
 * The run's last turn ended when its execution went idle, so `execution.since_ms` is the
 * reference. Only an idle, live run that finished at least one turn has a cooling cache.
 */
export function cacheSince(run: AgentRun | null | undefined): number | null {
  if (!run || run.ended_at_ms !== null) return null;
  if (run.execution.value !== 'idle' || run.turns_completed <= 0) return null;
  return run.execution.since_ms > 0 ? run.execution.since_ms : null;
}

/** Everything a chip needs, or null when there is no chip to show. */
export function runCacheStatus(run: AgentRun | null | undefined, nowMs: number, overrides: Readonly<Record<string, number>> = {}): (CacheStatus & { ttlMs: number; sinceMs: number }) | null {
  const since = cacheSince(run);
  if (since === null || !run) return null;
  const ttl = cacheTtlMs(run.harness, overrides);
  if (ttl === null) return null;
  const s = cacheStatus(since, nowMs, ttl);
  if (s.state === 'cold' && s.coldForMs > COLD_VISIBLE_MS) return null;
  return { ...s, ttlMs: ttl, sinceMs: since };
}

export interface SharedClockDeps {
  now(): number;
  visible: Readable<boolean>;
  setInterval(f: () => void, ms: number): unknown;
  clearInterval(h: unknown): void;
  ms?: number;
}

/**
 * One ticker for every chip. It runs only while somebody listens and the page is visible; it
 * takes a fresh reading when the page shows again.
 */
export function createSharedClock(d: SharedClockDeps): Readable<number> {
  const listeners = new Set<() => void>();
  let value = d.now();
  let timer: unknown = null;
  let offVisible: (() => void) | null = null;
  const tick = () => {
    value = d.now();
    for (const cb of [...listeners]) cb();
  };
  const sync = () => {
    const want = listeners.size > 0 && d.visible.getSnapshot();
    if (want && timer === null) {
      tick();
      timer = d.setInterval(tick, d.ms ?? 10_000);
    } else if (!want && timer !== null) {
      d.clearInterval(timer);
      timer = null;
    }
  };
  return {
    getSnapshot: () => value,
    subscribe(cb) {
      listeners.add(cb);
      if (listeners.size === 1) offVisible = d.visible.subscribe(sync);
      sync();
      return () => {
        listeners.delete(cb);
        if (listeners.size === 0) {
          offVisible?.();
          offVisible = null;
        }
        sync();
      };
    },
  };
}
