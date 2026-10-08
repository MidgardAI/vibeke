// Approval requests from panes (spec 09 §3.2 "Approved calls") on every own full-access host,
// kept live from `auth.approval_*` events and `auth.list` after every (re)connect. The inbox and
// the review screen read them; deciding goes through `decide` here so both show the same result.

import { useEffect } from 'react';
import { RpcError, type AppEvent, type ApprovalDecision, type ApprovalRequest } from '@vibeke/core';
import { errorMessage } from '../lib/answer';
import { applyApproval, approvalChange, decideOutcome, decideParams, openApprovals, type DecideResult } from '../lib/approvals';
import { isOwnFullHost } from '../lib/handoff-send';
import { ValueStore, useStore } from '../lib/store';
import { useApp } from './hooks';
import type { AppModel } from './model';

export interface HostApprovals {
  list: ApprovalRequest[];
  loaded: boolean;
  error: string | null;
}

const EMPTY: HostApprovals = { list: [], loaded: false, error: null };

/** A running approved call may take a while (a peer redeem waits up to 60 s on the host). */
const DECIDE_TIMEOUT_MS = 90_000;

/** An older host without approved calls: nothing there. */
const unknownMethod = (e: unknown): boolean => e instanceof RpcError && (e.kind === 'method_not_found' || e.code === -32601);

export class ApprovalStores {
  readonly hosts = new ValueStore<ReadonlyMap<string, HostApprovals>>(new Map());
  /** `host:request` being decided from this window. */
  readonly deciding = new ValueStore<ReadonlySet<string>>(new Set());
  private subs = new Map<string, { off: () => void; online: boolean }>();
  private refs = 0;
  private offManager: (() => void) | null = null;

  constructor(private readonly app: AppModel) {}

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
      if (online && !s.online) void this.refresh(id);
      s.online = online;
    }
    for (const [id, s] of this.subs) {
      if (seen.has(id)) continue;
      s.off();
      this.subs.delete(id);
      this.hosts.update((m) => {
        if (!m.has(id)) return m;
        const next = new Map(m);
        next.delete(id);
        return next;
      });
    }
  }

  async refresh(hostId: string): Promise<void> {
    const conn = this.app.conn(hostId);
    if (!conn) return;
    try {
      const r = await conn.request('auth.list', {});
      this.set(hostId, () => ({ list: openApprovals(r.approvals ?? []), loaded: true, error: null }));
    } catch (e) {
      this.set(hostId, (cur) => ({ ...cur, loaded: true, error: unknownMethod(e) ? null : errorMessage(e) }));
    }
  }

  get(hostId: string, request: string): ApprovalRequest | undefined {
    return this.hosts.get().get(hostId)?.list.find((r) => r.request === request);
  }

  /**
   * Decide one request (an explicit tap in the app; never from a notification). Toasts the
   * outcome and drops the request; a request gone on the host is dropped too. Returns the
   * host's answer, or null on an error (toasted).
   */
  async decide(hostId: string, r: ApprovalRequest, decision: ApprovalDecision): Promise<DecideResult | null> {
    const key = `${hostId}:${r.request}`;
    if (this.deciding.get().has(key)) return null;
    const conn = this.app.conn(hostId);
    if (!conn) return null;
    this.deciding.update((s) => new Set([...s, key]));
    try {
      const res = await conn.request('auth.approve.decide', decideParams(r, decision), { timeoutMs: DECIDE_TIMEOUT_MS });
      this.drop(hostId, r.request);
      const o = decideOutcome(res);
      this.app.haptic(o.tone === 'error' ? 'error' : 'success');
      this.app.toast(o.text, o.tone, o.tone === 'error' ? 8000 : 4000);
      return res;
    } catch (e) {
      if (e instanceof RpcError && (e.kind === 'not_found' || e.kind === 'conflict')) this.drop(hostId, r.request);
      this.app.haptic('error');
      this.app.toast(errorMessage(e), 'error', 8000);
      return null;
    } finally {
      this.deciding.update((s) => {
        const next = new Set(s);
        next.delete(key);
        return next;
      });
    }
  }

  private drop(hostId: string, request: string): void {
    this.set(hostId, (cur) => ({ ...cur, list: applyApproval(cur.list, { k: 'remove', id: request }) }));
  }

  private set(hostId: string, f: (cur: HostApprovals) => HostApprovals): void {
    this.hosts.update((m) => {
      const next = new Map(m);
      next.set(hostId, f(m.get(hostId) ?? EMPTY));
      return next;
    });
  }

  private onEvent(hostId: string, e: AppEvent): void {
    if (!e.type.startsWith('auth.approval_')) return;
    const c = approvalChange(e);
    if (!c) return;
    this.set(hostId, (cur) => ({ ...cur, list: openApprovals(applyApproval(cur.list, c)) }));
    // The event has the peer's id only; the listing has the full record (handle, peer name).
    if (c.k === 'upsert') void this.refresh(hostId);
  }
}

const stores = new WeakMap<AppModel, ApprovalStores>();

export function approvalStores(app: AppModel): ApprovalStores {
  let s = stores.get(app);
  if (!s) stores.set(app, (s = new ApprovalStores(app)));
  return s;
}

/** Keep approval requests live (mounted by the main window, and by screens using them). */
export function useApprovalStores(): ApprovalStores {
  const app = useApp();
  const s = approvalStores(app);
  useEffect(() => s.start(), [s]);
  return s;
}

export function useApprovals(): ReadonlyMap<string, HostApprovals> {
  return useStore(useApprovalStores().hosts);
}

export function useHostApprovals(hostId: string): HostApprovals {
  return useApprovals().get(hostId) ?? EMPTY;
}

/** Open approval requests on every own host (inbox badge). */
export function useApprovalCount(): number {
  let n = 0;
  for (const h of useApprovals().values()) n += h.list.length;
  return n;
}

export function useDeciding(): ReadonlySet<string> {
  return useStore(useApprovalStores().deciding);
}
