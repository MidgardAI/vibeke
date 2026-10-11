// Embedded preview views outlive the element that shows them (spec 06 B3.5). A preview moves
// between the Previews panel and the centre (Expand / Collapse); each place mounts its own
// placeholder. The native view stays open across the move and only gets new bounds, so the page
// does not reload. One registry per window (per `PreviewViewApi`): the first claim opens the view,
// and the view closes once nothing has claimed it for a short time.

import type { PreviewViewApi, PreviewViewState } from '../platform';

/** How long an unclaimed view stays open, so a placeholder mounted elsewhere can take it over. */
export const RELEASE_GRACE_MS = 1000;

export interface ViewRect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** What a placeholder shows: the view once it is open, the open error, the last pushed state. */
export interface ViewSlot {
  view: string | null;
  error: unknown;
  state: PreviewViewState | null;
}

export interface OpenParams {
  hostId: string;
  machine: string;
  url: string;
}

export interface Timers {
  set(fn: () => void, ms: number): unknown;
  clear(handle: unknown): void;
}

const realTimers: Timers = {
  set: (fn, ms) => setTimeout(fn, ms),
  clear: (h) => clearTimeout(h as ReturnType<typeof setTimeout>),
};

interface Entry {
  key: string;
  slot: ViewSlot;
  claims: number;
  timer: unknown;
  closed: boolean;
  rect: ViewRect;
}

type ViewApi = Pick<PreviewViewApi, 'open' | 'close' | 'bounds' | 'onState'>;

export class PreviewViewRegistry {
  private readonly entries = new Map<string, Entry>();
  private readonly listeners = new Set<() => void>();
  private offState: (() => void) | null = null;

  constructor(
    private readonly api: ViewApi,
    private readonly timers: Timers = realTimers,
    private readonly graceMs = RELEASE_GRACE_MS,
  ) {}

  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };

  /** The slot of `key` (a stable object until it changes), or null when no view is kept for it. */
  get(key: string): ViewSlot | null {
    return this.entries.get(key)?.slot ?? null;
  }

  /** Keys with a view kept open (claimed or in their grace time). */
  keys(): string[] {
    return [...this.entries.keys()];
  }

  /**
   * Claim the view of `key`, opening it with `params` when none is kept (or the last open failed).
   * Call the result when the placeholder goes away (idempotent).
   */
  claim(key: string, params: OpenParams): () => void {
    let e = this.entries.get(key);
    if (e && e.slot.error != null && !e.slot.view) {
      this.drop(e);
      e = undefined;
    }
    if (!e) {
      e = { key, slot: { view: null, error: null, state: null }, claims: 0, timer: null, closed: false, rect: { x: 0, y: 0, width: 0, height: 0 } };
      this.entries.set(key, e);
      this.watchState();
      this.open(e, params);
      this.emit();
    }
    e.claims++;
    if (e.timer != null) {
      this.timers.clear(e.timer);
      e.timer = null;
    }
    const entry = e;
    let done = false;
    return () => {
      if (done) return;
      done = true;
      this.release(entry);
    };
  }

  /** Place the view of `key` (the placeholder's rectangle) and show or hide it. */
  bounds(key: string, rect: ViewRect, visible: boolean): void {
    const e = this.entries.get(key);
    if (!e || !e.slot.view) return;
    e.rect = rect;
    this.api.bounds(e.slot.view, rect, visible);
  }

  private release(e: Entry): void {
    if (e.closed) return;
    e.claims = Math.max(0, e.claims - 1);
    if (e.claims > 0) return;
    // Nobody shows it now: hide it at once, and close it unless a placeholder claims it soon.
    if (e.slot.view) this.api.bounds(e.slot.view, e.rect, false);
    e.timer = this.timers.set(() => {
      e.timer = null;
      if (e.claims === 0) this.drop(e);
    }, this.graceMs);
  }

  private open(e: Entry, p: OpenParams): void {
    this.api
      .open(p)
      .then((r) => {
        if (e.closed) return this.api.close(r.view);
        this.update(e, { view: r.view });
      })
      .catch((err: unknown) => {
        if (!e.closed) this.update(e, { error: err ?? 'error' });
      });
  }

  private drop(e: Entry): void {
    if (e.closed) return;
    e.closed = true;
    if (e.timer != null) this.timers.clear(e.timer);
    e.timer = null;
    if (this.entries.get(e.key) === e) this.entries.delete(e.key);
    if (e.slot.view) this.api.close(e.slot.view);
    if (this.entries.size === 0) {
      this.offState?.();
      this.offState = null;
    }
    this.emit();
  }

  private watchState(): void {
    if (this.offState) return;
    this.offState = this.api.onState((s) => {
      for (const e of this.entries.values()) if (e.slot.view === s.view) this.update(e, { state: s });
    });
  }

  private update(e: Entry, patch: Partial<ViewSlot>): void {
    e.slot = { ...e.slot, ...patch };
    this.emit();
  }

  private emit(): void {
    for (const cb of [...this.listeners]) cb();
  }
}

const registries = new WeakMap<object, PreviewViewRegistry>();

/** The window's registry for `api` (one per platform object). */
export function registryFor(api: PreviewViewApi): PreviewViewRegistry {
  let r = registries.get(api);
  if (!r) {
    r = new PreviewViewRegistry(api);
    registries.set(api, r);
  }
  return r;
}
