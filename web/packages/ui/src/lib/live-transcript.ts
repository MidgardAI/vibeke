// Keeping the conversation's newest turns current (spec 16 §9.1): host events for the run
// (turn started / completed, state changes, usage, file edits, interactions) schedule a debounced
// `agent.transcript {limit: 2}`; the dashboard-change trigger and a slow safety poll back it up.
// `LatestFeed` owns the fetch loop: one request in flight per run, follow-ups coalesced, and every
// response fenced by the run it was asked for, so a slow answer for run A never lands on run B.

import type { AppEvent, HostManagerApi } from '@vibeke/core';

/** Debounce for event-triggered refetches (events come in bursts: state + turn + usage). */
export const EVENT_DEBOUNCE_MS = 200;
/** While the agent works and events flow, a slow poll covers changes no event announces. */
export const SAFETY_POLL_MS = 15_000;

const AGENT_TYPES = new Set(['agent.turn_started', 'agent.turn_completed', 'agent.state_changed', 'agent.usage', 'agent.file_changed', 'agent.session_ended', 'agent.exited', 'agent.rate_limited']);

/** An event that may change what the run's newest turns show. */
export function isRunEvent(e: AppEvent, run: string, pane: string | null): boolean {
  if (!(AGENT_TYPES.has(e.type) || e.type.startsWith('interaction.'))) return false;
  const s = e.subject ?? {};
  return s.run === run || (!!pane && s.pane === pane);
}

export interface FeedClock {
  setTimeout(fn: () => void, ms: number): unknown;
  clearTimeout(h: unknown): void;
}

const realClock: FeedClock = {
  setTimeout: (fn, ms) => setTimeout(fn, ms),
  clearTimeout: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
};

export interface LatestFeedOptions<R> {
  fetch(run: string): Promise<R>;
  /** A response for `run` (only ever the current run). */
  apply(run: string, r: R): void;
  fail(run: string, e: unknown): void;
  clock?: FeedClock;
  debounceMs?: number;
}

export class LatestFeed<R> {
  private run: string | null = null;
  /** Bumped per run (and on dispose): responses and follow-ups of older generations are dropped. */
  private gen = 0;
  private inflight = false;
  private again = false;
  private timer: unknown = null;
  private readonly clock: FeedClock;

  constructor(private readonly o: LatestFeedOptions<R>) {
    this.clock = o.clock ?? realClock;
  }

  /** Switch to `run` and load it now (pending work for the previous run is abandoned). */
  setRun(run: string): void {
    this.gen++;
    this.run = run;
    this.inflight = false;
    this.again = false;
    this.cancelTimer();
    void this.load();
  }

  /** Refetch soon (debounced; a burst of triggers costs one request). */
  schedule(ms = this.o.debounceMs ?? EVENT_DEBOUNCE_MS): void {
    if (this.run === null) return;
    this.cancelTimer();
    const g = this.gen;
    this.timer = this.clock.setTimeout(() => {
      this.timer = null;
      if (g === this.gen) void this.load();
    }, ms);
  }

  /** Fetch now, or once more after the request in flight. */
  async load(): Promise<void> {
    const run = this.run;
    if (run === null) return;
    if (this.inflight) {
      this.again = true;
      return;
    }
    const g = this.gen;
    this.inflight = true;
    try {
      const r = await this.o.fetch(run);
      if (g === this.gen) this.o.apply(run, r);
    } catch (e) {
      if (g === this.gen) this.o.fail(run, e);
    } finally {
      if (g === this.gen) {
        this.inflight = false;
        if (this.again) {
          this.again = false;
          void this.load();
        }
      }
    }
  }

  dispose(): void {
    this.gen++;
    this.run = null;
    this.cancelTimer();
  }

  private cancelTimer(): void {
    if (this.timer !== null) this.clock.clearTimeout(this.timer);
    this.timer = null;
  }
}

/**
 * Listen to `hostId`'s events for one run: `onHit` for each relevant event. Returns the
 * unsubscribe, or null when the manager has no event stream (then polling stays the only source).
 */
export function watchRunEvents(manager: Pick<HostManagerApi, 'subscribeEvents'> | null | undefined, hostId: string, run: string, pane: string | null, onHit: (e: AppEvent) => void): (() => void) | null {
  if (!manager || typeof manager.subscribeEvents !== 'function') return null;
  return manager.subscribeEvents(hostId, (e) => {
    if (isRunEvent(e, run, pane)) onHit(e);
  });
}
