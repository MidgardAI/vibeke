// Handoffs in the background (spec 16 §15.2): incoming handoffs of every own host (nav badge,
// the Handoffs screen) and outgoing jobs (`handoff.job`), kept live from host events. The send
// sheet may close while a job runs: jobs started from this window end in a toast unless the
// sheet still shows them.

import { useEffect } from 'react';
import { RpcError, type AppEvent, type HandoffJob, type IncomingHandoff } from '@vibeke/core';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { isOwnFullHost, jobFinal, jobFromEvent, jobView, upsertJob } from '../lib/handoff-send';
import { actionCount, applyIncoming, incomingChange } from '../lib/incoming';
import { ValueStore, useStore } from '../lib/store';
import { useApp } from './hooks';
import type { AppModel } from './model';

export interface HostIncoming {
  list: IncomingHandoff[];
  loaded: boolean;
  error: string | null;
  /** Latest `handoff.updated` phase per record (`cloning`, `importing`, `starting`). */
  phase: Record<string, string>;
}

const EMPTY: HostIncoming = { list: [], loaded: false, error: null, phase: {} };

/** The host does not know the method (an older server): treat as "nothing there". */
const unknownMethod = (e: unknown): boolean => e instanceof RpcError && (e.kind === 'method_not_found' || e.code === -32601);

export class HandoffStores {
  /** Per own full-access host. */
  readonly incoming = new ValueStore<ReadonlyMap<string, HostIncoming>>(new Map());
  /** Outgoing jobs per source host. */
  readonly jobs = new ValueStore<ReadonlyMap<string, HandoffJob[]>>(new Map());
  private subs = new Map<string, { off: () => void; online: boolean }>();
  /** `host:job` started from this window. */
  private mine = new Set<string>();
  /** `host:job` a sheet is showing (it reports the outcome itself). */
  private watched = new Map<string, number>();
  private refs = 0;
  private offManager: (() => void) | null = null;

  constructor(private readonly app: AppModel) {}

  /** Keep the stores live while anything uses them (ref-counted). */
  start(): () => void {
    if (this.refs++ === 0) {
      this.offManager = this.app.manager.subscribe(() => this.sync());
      this.sync();
    }
    let done = false;
    return () => {
      if (done) return;
      done = true;
      if (--this.refs > 0) return;
      this.offManager?.();
      this.offManager = null;
      for (const s of this.subs.values()) s.off();
      this.subs.clear();
    };
  }

  private sync(): void {
    const seen = new Set<string>();
    for (const h of this.app.manager.getSnapshot()) {
      if (!isOwnFullHost(h)) continue;
      const id = h.record.host_id;
      seen.add(id);
      const online = h.status === 'online';
      let s = this.subs.get(id);
      if (!s) {
        s = { off: this.app.manager.subscribeEvents(id, (e) => this.onEvent(id, e)), online: false };
        this.subs.set(id, s);
      }
      // (Re)connected: events may have been missed.
      if (online && !s.online) {
        void this.refreshIncoming(id);
        void this.refreshJobs(id);
      }
      s.online = online;
    }
    for (const [id, s] of this.subs) {
      if (seen.has(id)) continue;
      s.off();
      this.subs.delete(id);
      this.incoming.update((m) => without(m, id));
      this.jobs.update((m) => without(m, id));
    }
  }

  async refreshIncoming(hostId: string): Promise<void> {
    const conn = this.app.conn(hostId);
    if (!conn) return;
    try {
      const r = await conn.request('handoff.incoming.list', {});
      this.setIncoming(hostId, (cur) => ({ ...cur, list: r.incoming ?? [], loaded: true, error: null }));
    } catch (e) {
      this.setIncoming(hostId, (cur) => ({ ...cur, loaded: true, error: unknownMethod(e) ? null : errorMessage(e) }));
    }
  }

  async refreshJobs(hostId: string): Promise<void> {
    const conn = this.app.conn(hostId);
    if (!conn) return;
    try {
      const r = await conn.request('handoff.jobs', {});
      const fresh = r.jobs ?? [];
      this.jobs.update((m) => {
        const next = new Map(m);
        let list = m.get(hostId) ?? [];
        for (const j of fresh) list = upsertJob(list, j);
        next.set(hostId, list);
        return next;
      });
    } catch {
      // Older hosts have no jobs; events still arrive.
    }
  }

  /** A record changed by this window (accept/decline result): show it before its event lands. */
  putIncoming(hostId: string, record: IncomingHandoff): void {
    this.setIncoming(hostId, (cur) => ({ ...cur, list: applyIncoming(cur.list, { k: 'upsert', record, phase: null }) }));
  }

  /** A job this window started (the `handoff.send` result). */
  trackJob(hostId: string, job: HandoffJob): void {
    this.mine.add(`${hostId}:${job.id}`);
    this.putJob(hostId, job);
  }

  /** A sheet shows this job: no toast for it meanwhile. Returns the release. */
  watch(hostId: string, jobId: string): () => void {
    const k = `${hostId}:${jobId}`;
    this.watched.set(k, (this.watched.get(k) ?? 0) + 1);
    let done = false;
    return () => {
      if (done) return;
      done = true;
      const n = (this.watched.get(k) ?? 1) - 1;
      if (n > 0) this.watched.set(k, n);
      else this.watched.delete(k);
    };
  }

  job(hostId: string, jobId: string): HandoffJob | undefined {
    return this.jobs.get().get(hostId)?.find((j) => j.id === jobId);
  }

  private putJob(hostId: string, job: HandoffJob): void {
    this.jobs.update((m) => {
      const next = new Map(m);
      next.set(hostId, upsertJob(m.get(hostId) ?? [], job));
      return next;
    });
  }

  private setIncoming(hostId: string, f: (cur: HostIncoming) => HostIncoming): void {
    this.incoming.update((m) => {
      const next = new Map(m);
      next.set(hostId, f(m.get(hostId) ?? EMPTY));
      return next;
    });
  }

  private onEvent(hostId: string, e: AppEvent): void {
    if (!e.type.startsWith('handoff.')) return;
    const job = jobFromEvent(e);
    if (job) {
      const before = this.job(hostId, job.id);
      this.putJob(hostId, job);
      this.maybeToast(hostId, job, before);
      return;
    }
    const c = incomingChange(e);
    if (!c) return;
    if (c.k === 'refetch') {
      void this.refreshIncoming(hostId);
      return;
    }
    this.setIncoming(hostId, (cur) => {
      const phase = { ...cur.phase };
      if (c.k === 'upsert') {
        if (c.phase && c.record.state === 'importing') phase[c.record.id] = c.phase;
        else delete phase[c.record.id];
      } else delete phase[c.id];
      return { ...cur, list: applyIncoming(cur.list, c), phase };
    });
  }

  private maybeToast(hostId: string, job: HandoffJob, before: HandoffJob | undefined): void {
    const k = `${hostId}:${job.id}`;
    if (!this.mine.has(k) || this.watched.has(k) || !jobFinal(job)) return;
    if (before && jobFinal(before) && before.state === job.state && before.incoming_state === job.incoming_state) return;
    const v = jobView(job);
    const name = job.peer_name || job.peer;
    switch (v.phase) {
      case 'imported':
        this.app.toast(t.handoff.toastImported(name), 'ok', 5000);
        break;
      case 'importing':
        break;
      case 'failed':
        this.app.toast(t.handoff.toastFailed(name, v.error ?? ''), 'error', 8000);
        break;
      case 'cancelled':
        this.app.toast(t.handoff.cancelled);
        break;
      default:
        this.app.toast(t.handoff.toastDelivered(name), 'ok', 5000);
    }
    if (job.state !== 'delivered' || job.incoming_state !== 'importing') this.mine.delete(k);
  }
}

function without<V>(m: ReadonlyMap<string, V>, key: string): ReadonlyMap<string, V> {
  if (!m.has(key)) return m;
  const next = new Map(m);
  next.delete(key);
  return next;
}

const stores = new WeakMap<AppModel, HandoffStores>();

export function handoffStores(app: AppModel): HandoffStores {
  let s = stores.get(app);
  if (!s) stores.set(app, (s = new HandoffStores(app)));
  return s;
}

/** Keep incoming handoffs and jobs live (mounted by the main window, and by screens using them). */
export function useHandoffStores(): HandoffStores {
  const app = useApp();
  const s = handoffStores(app);
  useEffect(() => s.start(), [s]);
  return s;
}

export function useIncoming(): ReadonlyMap<string, HostIncoming> {
  return useStore(useHandoffStores().incoming);
}

export function useHostIncoming(hostId: string): HostIncoming {
  return useIncoming().get(hostId) ?? EMPTY;
}

/** Handoffs waiting for the user (pending or failed) on every own host. */
export function useIncomingCount(): number {
  const m = useIncoming();
  return actionCount([...m.values()].map((h) => h.list));
}

export function useJobs(): ReadonlyMap<string, HandoffJob[]> {
  return useStore(useHandoffStores().jobs);
}
