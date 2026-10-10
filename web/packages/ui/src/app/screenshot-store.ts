// Screenshots per workspace, kept live from host events: the list the Screenshots tab shows and
// the unread count behind its badge. A workspace is "watched" while a screen shows it (the
// workspace screen mounts the hook, the tab shares it). `screenshot.captured` and
// `screenshot.deleted` are hints: the list is refetched, so a missed event only costs a refresh,
// and a reconnect refetches too. "Last seen" is remembered per workspace in localStorage.

import { useEffect } from 'react';
import type { AppEvent, ScreenshotMeta } from '@vibeke/core';
import { capturedHint, deletedIds, listParams, mergeShots, seenMark, unreadSince, withoutShots } from '../lib/screenshots';
import { ValueStore, useStore } from '../lib/store';
import { useApp } from './hooks';
import type { AppModel } from './model';

export interface WorkspaceShots {
  list: ScreenshotMeta[];
  loaded: boolean;
  error: boolean;
  /** Ids captured since the tab was last open. */
  unread: ReadonlySet<string>;
}

const EMPTY: WorkspaceShots = { list: [], loaded: false, error: false, unread: new Set() };
const SEEN_PREFIX = 'vk.shots.seen.';
const REFRESH_DELAY_MS = 250;

const key = (host: string, ws: string) => `${host}/${ws}`;

function readSeen(k: string): number | null {
  try {
    const v = Number(globalThis.localStorage?.getItem(SEEN_PREFIX + k));
    return Number.isFinite(v) && v > 0 ? v : null;
  } catch {
    return null;
  }
}

function writeSeen(k: string, ms: number): void {
  try {
    globalThis.localStorage?.setItem(SEEN_PREFIX + k, String(ms));
  } catch {
    // storage blocked: unread just restarts empty
  }
}

interface Watch {
  host: string;
  ws: string;
  refs: number;
  /** The tab is open: new screenshots count as seen. */
  open: number;
  timer: ReturnType<typeof setTimeout> | null;
  /** `seen` as read from storage when the first list arrived (null once applied). */
  baseline: number | null | undefined;
}

export class ScreenshotStore {
  readonly state = new ValueStore<ReadonlyMap<string, WorkspaceShots>>(new Map());
  private watches = new Map<string, Watch>();
  private subs = new Map<string, { off: () => void; online: boolean }>();
  private flights = new Map<string, Promise<void>>();
  private queued = new Set<string>();
  private offManager: (() => void) | null = null;

  constructor(private readonly app: AppModel) {}

  get(host: string, ws: string): WorkspaceShots {
    return this.state.get().get(key(host, ws)) ?? EMPTY;
  }

  /** Keep `host/ws` live (ref-counted). */
  watch(host: string, ws: string): () => void {
    const k = key(host, ws);
    let w = this.watches.get(k);
    if (!w) {
      w = { host, ws, refs: 0, open: 0, timer: null, baseline: undefined };
      this.watches.set(k, w);
    }
    w.refs++;
    if (!this.offManager) this.offManager = this.app.manager.subscribe(() => this.sync());
    this.sync();
    void this.refresh(host, ws);
    let done = false;
    return () => {
      if (done) return;
      done = true;
      const cur = this.watches.get(k);
      if (!cur || --cur.refs > 0) return;
      if (cur.timer) clearTimeout(cur.timer);
      this.watches.delete(k);
      this.sync();
      if (!this.watches.size) {
        this.offManager?.();
        this.offManager = null;
      }
    };
  }

  /** The Screenshots tab is showing (`on`): nothing is unread while it is. Ref-counted. */
  setOpen(host: string, ws: string, on: boolean): void {
    const w = this.watches.get(key(host, ws));
    if (!w) return;
    w.open = Math.max(0, w.open + (on ? 1 : -1));
    if (on) this.markSeen(host, ws);
  }

  markSeen(host: string, ws: string): void {
    const k = key(host, ws);
    const cur = this.state.get().get(k);
    const mark = seenMark(cur?.list ?? [], this.app.platform.clock.now());
    writeSeen(k, mark);
    if (cur && cur.unread.size) this.put(k, { ...cur, unread: new Set() });
  }

  /**
   * Refetch the list. One request per workspace is in flight: a call meanwhile queues exactly one
   * follow-up, so an older response can never overwrite a newer one.
   */
  refresh(host: string, ws: string): Promise<void> {
    const k = key(host, ws);
    const cur = this.flights.get(k);
    if (cur) {
      this.queued.add(k);
      return cur;
    }
    const run = (async () => {
      try {
        do {
          this.queued.delete(k);
          await this.fetchList(host, ws);
        } while (this.queued.has(k));
      } finally {
        this.flights.delete(k);
      }
    })();
    this.flights.set(k, run);
    return run;
  }

  private async fetchList(host: string, ws: string): Promise<void> {
    const conn = this.app.conn(host);
    if (!conn) return;
    const k = key(host, ws);
    try {
      const hs = this.app.manager.getSnapshot().find((h) => h.record.host_id === host);
      const limit = hs?.record.limit ?? hs?.info?.limit;
      const r = await conn.request('screenshot.list', listParams(limit, ws));
      const w = this.watches.get(k);
      if (!w) return;
      const cur = this.state.get().get(k) ?? EMPTY;
      const list = r.screenshots ?? [];
      let unread = new Set(cur.unread);
      if (w.baseline === undefined) {
        // First list: whatever arrived since the tab was last open is unread.
        w.baseline = readSeen(k);
        unread = unreadSince(list, w.baseline);
        if (w.baseline === null) writeSeen(k, seenMark(list, this.app.platform.clock.now()));
      } else {
        for (const s of list) if (!cur.list.some((o) => o.id === s.id)) unread.add(s.id);
      }
      if (w.open > 0) {
        unread = new Set();
        writeSeen(k, seenMark(list, this.app.platform.clock.now()));
      }
      for (const id of unread) if (!list.some((s) => s.id === id)) unread.delete(id);
      this.put(k, { list: mergeShots([], list), loaded: true, error: false, unread });
    } catch {
      const cur = this.state.get().get(k) ?? EMPTY;
      this.put(k, { ...cur, loaded: true, error: !cur.list.length });
    }
  }

  private put(k: string, v: WorkspaceShots): void {
    this.state.update((m) => new Map(m).set(k, v));
  }

  private sync(): void {
    const hosts = new Set([...this.watches.values()].map((w) => w.host));
    const status = new Map(this.app.manager.getSnapshot().map((h) => [h.record.host_id, h.status === 'online']));
    for (const id of hosts) {
      const online = status.get(id) ?? false;
      let s = this.subs.get(id);
      if (!s) {
        s = { off: this.app.manager.subscribeEvents(id, (e) => this.onEvent(id, e)), online: false };
        this.subs.set(id, s);
      }
      // (Re)connected: events may have been missed.
      if (online && !s.online) for (const w of this.watches.values()) if (w.host === id) void this.refresh(w.host, w.ws);
      s.online = online;
    }
    for (const [id, s] of this.subs) {
      if (hosts.has(id)) continue;
      s.off();
      this.subs.delete(id);
    }
  }

  private onEvent(host: string, e: AppEvent): void {
    const gone = deletedIds(e);
    if (gone) {
      const ids = new Set(gone);
      this.state.update((m) => {
        const next = new Map(m);
        for (const [k, v] of m) if (k.startsWith(`${host}/`) && v.list.some((s) => ids.has(s.id))) next.set(k, { ...v, list: withoutShots(v.list, ids), unread: new Set([...v.unread].filter((i) => !ids.has(i))) });
        return next;
      });
      return;
    }
    const hint = capturedHint(e);
    if (!hint) return;
    for (const w of this.watches.values()) {
      if (w.host !== host || (hint.workspace && hint.workspace !== w.ws)) continue;
      if (w.timer) clearTimeout(w.timer);
      w.timer = setTimeout(() => {
        w.timer = null;
        void this.refresh(w.host, w.ws);
      }, REFRESH_DELAY_MS);
    }
  }
}

const stores = new WeakMap<AppModel, ScreenshotStore>();

export function screenshotStore(app: AppModel): ScreenshotStore {
  let s = stores.get(app);
  if (!s) stores.set(app, (s = new ScreenshotStore(app)));
  return s;
}

/** The workspace's screenshots, live while the caller is mounted. */
export function useWorkspaceShots(host: string | null, ws: string | null): WorkspaceShots {
  const app = useApp();
  const s = screenshotStore(app);
  const all = useStore(s.state);
  useEffect(() => (host && ws ? s.watch(host, ws) : undefined), [s, host, ws]);
  return (host && ws && all.get(key(host, ws))) || EMPTY;
}
