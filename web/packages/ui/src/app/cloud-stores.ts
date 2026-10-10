// Cloud sandboxes in the background (spec 17): providers, boxes and move jobs of every own
// full-access host, kept live from `cloud.job`, `cloud.box.changed` and `cloud.auth.changed`
// events, with a slow poll as the fallback when events do not arrive. Modeled on handoff-stores.

import { useEffect } from 'react';
import { RpcError, type AppEvent, type CloudBox, type CloudJob, type CloudProvider } from '@vibeke/core';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { cloudBoxFromEvent, cloudJobError, cloudJobFinal, cloudJobFromEvent, upsertCloudBox, upsertCloudJob } from '../lib/cloud';
import { isOwnFullHost } from '../lib/handoff-send';
import { ValueStore, useStore } from '../lib/store';
import { useApp } from './hooks';
import type { AppModel } from './model';

export interface HostCloud {
  providers: CloudProvider[];
  boxes: CloudBox[];
  /** Providers that did not answer the last `cloud.box.list`. */
  errors: { provider: string; kind: string; message: string }[];
  jobs: CloudJob[];
  loaded: boolean;
  /** Set when the host cannot answer (an older server without cloud methods reads as empty). */
  error: string | null;
  /** The host has no cloud methods at all. */
  unsupported: boolean;
}

const EMPTY: HostCloud = { providers: [], boxes: [], errors: [], jobs: [], loaded: false, error: null, unsupported: false };
const POLL_MS = 30_000;

const unknownMethod = (e: unknown): boolean => e instanceof RpcError && (e.kind === 'method_not_found' || e.code === -32601);

export class CloudStores {
  readonly hosts = new ValueStore<ReadonlyMap<string, HostCloud>>(new Map());
  private subs = new Map<string, { off: () => void; online: boolean }>();
  /** `host:job` started from this window, with the hold that keeps the store live until it ends. */
  private mine = new Map<string, () => void>();
  /** `host:job` a sheet is showing (it reports the outcome itself). */
  private watched = new Map<string, number>();
  private refs = 0;
  private offManager: (() => void) | null = null;
  private timer: ReturnType<typeof setInterval> | null = null;

  constructor(private readonly app: AppModel) {}

  start(): () => void {
    if (this.refs++ === 0) {
      this.offManager = this.app.manager.subscribe(() => this.sync());
      this.timer = setInterval(() => this.poll(), POLL_MS);
      this.sync();
    }
    let done = false;
    return () => {
      if (done) return;
      done = true;
      if (--this.refs > 0) return;
      this.offManager?.();
      this.offManager = null;
      if (this.timer) clearInterval(this.timer);
      this.timer = null;
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
      if (online && !s.online) void this.refresh(id);
      s.online = online;
    }
    for (const [id, s] of this.subs) {
      if (seen.has(id)) continue;
      s.off();
      this.subs.delete(id);
      this.hosts.update((m) => {
        const next = new Map(m);
        next.delete(id);
        return next;
      });
    }
  }

  private poll(): void {
    for (const [id, s] of this.subs) if (s.online) void this.refresh(id);
  }

  private set(hostId: string, f: (cur: HostCloud) => HostCloud): void {
    this.hosts.update((m) => {
      const next = new Map(m);
      next.set(hostId, f(m.get(hostId) ?? EMPTY));
      return next;
    });
  }

  /** Providers, boxes and jobs of one host. `refresh` asks the host to re-list at the providers. */
  async refresh(hostId: string, opts: { refresh?: boolean; verify?: boolean } = {}): Promise<void> {
    const conn = this.app.conn(hostId);
    if (!conn) return;
    const [p, b, j] = await Promise.allSettled([
      conn.request('cloud.providers', opts.verify ? { verify: true } : {}),
      conn.request('cloud.box.list', opts.refresh ? { refresh: true } : {}, { timeoutMs: 60_000 }),
      conn.request('cloud.jobs', {}),
    ]);
    if (p.status === 'rejected' && unknownMethod(p.reason)) {
      this.set(hostId, (cur) => ({ ...cur, loaded: true, unsupported: true, error: null }));
      return;
    }
    const before = this.hosts.get().get(hostId)?.jobs ?? [];
    this.set(hostId, (cur) => {
      let jobs = cur.jobs;
      if (j.status === 'fulfilled') for (const x of j.value.jobs ?? []) jobs = upsertCloudJob(jobs, x);
      return {
        providers: p.status === 'fulfilled' ? (p.value.providers ?? []) : cur.providers,
        boxes: b.status === 'fulfilled' ? (b.value.boxes ?? []) : cur.boxes,
        errors: b.status === 'fulfilled' ? (b.value.errors ?? []) : cur.errors,
        jobs,
        loaded: true,
        unsupported: false,
        error: p.status === 'rejected' ? errorMessage(p.reason) : b.status === 'rejected' ? errorMessage(b.reason) : null,
      };
    });
    // A missed event: the poll settles this window's jobs too.
    if (j.status === 'fulfilled')
      for (const x of j.value.jobs ?? []) {
        const now = this.job(hostId, x.id);
        if (now) this.settle(hostId, now, before.find((y) => y.id === x.id));
      }
  }

  async refreshProviders(hostId: string): Promise<void> {
    const conn = this.app.conn(hostId);
    if (!conn) return;
    try {
      const r = await conn.request('cloud.providers', {});
      this.set(hostId, (cur) => ({ ...cur, providers: r.providers ?? [] }));
    } catch {
      // The next poll tries again.
    }
  }

  /**
   * A job this window started (the `cloud.move` result). The store stays live (events and the
   * poll) until the job ends, even when every sheet closed, so its outcome still shows a toast.
   */
  trackJob(hostId: string, job: CloudJob): void {
    const k = `${hostId}:${job.id}`;
    if (!this.mine.has(k)) this.mine.set(k, this.start());
    const before = this.job(hostId, job.id);
    this.putJob(hostId, job);
    const now = this.job(hostId, job.id);
    if (now) this.settle(hostId, now, before);
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

  job(hostId: string, jobId: string): CloudJob | undefined {
    return this.hosts.get().get(hostId)?.jobs.find((j) => j.id === jobId);
  }

  /** Show a box the host just returned before its event lands. */
  putBox(hostId: string, box: CloudBox): void {
    this.set(hostId, (cur) => ({ ...cur, boxes: upsertCloudBox(cur.boxes, box) }));
  }

  dropBox(hostId: string, box: string): void {
    this.set(hostId, (cur) => ({ ...cur, boxes: cur.boxes.filter((b) => b.box !== box) }));
  }

  private putJob(hostId: string, job: CloudJob): void {
    this.set(hostId, (cur) => ({ ...cur, jobs: upsertCloudJob(cur.jobs, job) }));
  }

  private onEvent(hostId: string, e: AppEvent): void {
    if (!e.type.startsWith('cloud.')) return;
    const job = cloudJobFromEvent(e);
    if (job) {
      const before = this.job(hostId, job.id);
      this.putJob(hostId, job);
      this.settle(hostId, job, before);
      return;
    }
    const box = cloudBoxFromEvent(e);
    if (box) {
      this.putBox(hostId, box);
      return;
    }
    if (e.type === 'cloud.auth.changed') void this.refreshProviders(hostId);
  }

  /** A job of this window ended: a toast unless a sheet shows it, and the hold is released. */
  private settle(hostId: string, job: CloudJob, before: CloudJob | undefined): void {
    const k = `${hostId}:${job.id}`;
    const hold = this.mine.get(k);
    if (!hold || !cloudJobFinal(job)) return;
    this.mine.delete(k);
    hold();
    if (this.watched.has(k)) return;
    if (before && cloudJobFinal(before) && before.state === job.state) return;
    if (job.state === 'done') this.app.toast(t.cloud.toastDone, 'ok', 5000);
    else if (job.state === 'failed') this.app.toast(`${t.cloud.states['failed'] ?? ''}: ${cloudJobError(job) ?? ''}`, 'error', 8000);
    else this.app.toast(t.cloud.states['cancelled'] ?? '');
  }
}

const stores = new WeakMap<AppModel, CloudStores>();

export function cloudStores(app: AppModel): CloudStores {
  let s = stores.get(app);
  if (!s) stores.set(app, (s = new CloudStores(app)));
  return s;
}

/** Keep cloud state live while a screen or sheet uses it. */
export function useCloudStores(): CloudStores {
  const app = useApp();
  const s = cloudStores(app);
  useEffect(() => s.start(), [s]);
  return s;
}

export function useCloud(): ReadonlyMap<string, HostCloud> {
  return useStore(useCloudStores().hosts);
}

export function useHostCloud(hostId: string): HostCloud {
  return useCloud().get(hostId) ?? EMPTY;
}
