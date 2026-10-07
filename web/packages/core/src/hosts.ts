// Multi-host manager (spec 16 §4.4, §5, §7.5, §9.3): one live connection per paired host, with
// reconnect/backoff, visibility hooks, event-stream resume by cursor and a subscribable
// snapshot per host (shaped for React's useSyncExternalStore).
//
// Dashboard state is refetched (debounced) when events arrive rather than patched locally:
// gateway events carry ids and deltas, not whole entities, and `dashboard.get` is the one
// authoritative shape. Event listeners still see every event, in order, deduped by seq.

import { Channel, ChannelError } from './channel';
import { helloDevice } from './hello';
import * as b64 from './b64';
import { recordKey } from './keys';
import {
  MUTATING_METHODS,
  normalizeDashboard,
  type AppApi,
  type AppEvent,
  type AppMethod,
  type Dashboard,
  type Scope,
} from './model';
import type { HostKind } from './link';
import { relayConnectUrl } from './pairing';
import type { Platform, TimerHandle } from './platform';
import { RpcClient, type RequestOptions } from './rpc';

/** A paired host as persisted by the device. */
export interface HostRecord {
  host_id: string;
  /** Relay WebSocket base URL. */
  relay: string;
  /** Pinned host Noise static public key (base64url). */
  hk: string;
  device_id: string;
  name: string;
  scope: Scope;
  paired_at?: number;
  /**
   * `device` (own pairing, default), `share` (someone shared a workspace/pane with us; limited and
   * expiring) or `handoff` (a handoff invitation: the host only accepts `handoff.begin/write/finish`,
   * so it never appears in dashboards or the inbox).
   */
  kind?: HostKind;
  /** Unix seconds after which a share/handoff device no longer works. */
  until?: number;
  /** Share label from the invitation. */
  label?: string | null;
  /** What a share covers. */
  limit?: { workspace?: string; pane?: string } | null;
  /**
   * Keystore name of this record's own private key (share/handoff invitations get one each, see
   * `createInvitationKey`); absent: the device key.
   */
  key?: string;
}

export const hostKind = (r: HostRecord): HostKind => r.kind ?? 'device';
/** A share/handoff host whose time is up (the gateway refuses it at the handshake). */
export const hostExpired = (r: HostRecord, nowMs: number): boolean => r.until !== undefined && nowMs / 1000 >= r.until;
/** Hosts that carry dashboards, panes and interactions (not handoff-only invitations). */
export const isDashboardHost = (r: HostRecord): boolean => hostKind(r) !== 'handoff';

export interface HostStore {
  list(): Promise<HostRecord[]>;
  put(record: HostRecord): Promise<void>;
  remove(hostId: string): Promise<void>;
}

export type HostStatus =
  | 'idle' // not started
  | 'connecting'
  | 'online'
  | 'offline' // relay unreachable / host offline; retrying
  | 'unauthorized' // plaintext unauthorized (unauthenticated hint); retrying
  | 'revoked' // authenticated device.revoked, or 3 consecutive unauthorized closes
  | 'incompatible' // unsupported_version
  | 'expired'; // share/handoff device past its `until`

export interface HostInfo {
  host_name: string;
  device_id: string;
  scope: Scope;
  server_version: string;
  features: string[];
  /** This device's kind on the host (`device` | `share` | `handoff`). */
  kind?: string;
  /** Unix seconds this device stops working; null for an ordinary device. */
  expires_at?: number | null;
  limit?: { workspace?: string | null; pane?: string | null } | null;
}

const HOST_KINDS: readonly string[] = ['device', 'share', 'handoff'];

/**
 * The record as the host describes this device in `hello` (kind, expiry, limit), or null when
 * nothing changed (or the gateway is too old to say). The host is authoritative: it may extend
 * or shorten a share, and the pairing link could only tell what was offered.
 */
export function recordFromHello(rec: HostRecord, info: Partial<HostInfo>): HostRecord | null {
  if (typeof info.kind !== 'string' || !HOST_KINDS.includes(info.kind)) return null;
  const next: HostRecord = { ...rec, kind: info.kind as HostKind };
  if (typeof info.expires_at === 'number' && Number.isFinite(info.expires_at)) next.until = info.expires_at;
  else if (info.expires_at === null) delete next.until;
  if ('limit' in info) {
    const l = info.limit;
    const limit = l && typeof l === 'object' ? { ...(l.workspace ? { workspace: l.workspace } : {}), ...(l.pane ? { pane: l.pane } : {}) } : null;
    if (limit && Object.keys(limit).length) next.limit = limit;
    else delete next.limit;
  }
  const same = (rec.kind ?? 'device') === next.kind && rec.until === next.until && JSON.stringify(rec.limit ?? null) === JSON.stringify(next.limit ?? null);
  return same ? null : next;
}

export interface HostState {
  record: HostRecord;
  status: HostStatus;
  /** Human-readable last error (null when healthy). */
  error: string | null;
  /** Relay close code of the last disconnect, if any (untrusted hint). */
  closeCode: number | null;
  info: HostInfo | null;
  dashboard: Dashboard | null;
  /** Last event seq seen (resume cursor). */
  cursor: number | null;
  lastOnlineAt: number | null;
  nextRetryAt: number | null;
}

/** The host is not connected; the request was not sent (outcome known). */
export class NotConnectedError extends Error {
  constructor(readonly hostId: string) {
    super(`host ${hostId} is not connected`);
    this.name = 'NotConnectedError';
  }
}

export interface ClientIdentity {
  client: string;
  version: string;
}

export interface ConnectionOptions {
  platform: Platform;
  devicePrivate: Uint8Array;
  client: ClientIdentity;
  backoffMinMs?: number;
  backoffMaxMs?: number;
  /** Debounce for dashboard refetches triggered by events. */
  refreshDebounceMs?: number;
  /** Persist a record the host updated (hello kind/expiry/limit). */
  persist?(record: HostRecord): Promise<void>;
}

const REFRESH_PREFIXES = ['agent.', 'interaction.', 'pane.', 'tab.', 'workspace.', 'task.', 'notification.', 'session.'];
const UNAUTHORIZED_LIMIT = 3;
/** A queued dashboard follow-up after a failed fetch: backoff 250 ms, 500 ms, … (≤ 5 s), ≤ 5 retries. */
const REFRESH_BACKOFF_MS = 250;
const REFRESH_BACKOFF_MAX_MS = 5_000;
const REFRESH_MAX_RETRIES = 5;

export class HostConnection implements HostConnectionApi {
  private state: HostState;
  private readonly listeners = new Set<() => void>();
  private readonly eventListeners = new Set<(e: AppEvent) => void>();
  private rpc: RpcClient | null = null;
  private retryTimer: TimerHandle | null = null;
  private refreshTimer: TimerHandle | null = null;
  private expiryTimer: TimerHandle | null = null;
  private attempts = 0;
  private unauthorizedCount = 0;
  private running = false;
  /** Incremented per connect attempt so stale async continuations are ignored. */
  private generation = 0;
  /**
   * Bumped whenever an event makes the dashboard stale; `refresh` records the value it fetched
   * against. A reconnect refetches while they differ, so a debounced refresh cancelled by a
   * disconnect cannot leave an outdated dashboard marked online.
   */
  private dirtyRev = 0;
  private cleanRev = 0;
  /** The in-flight refresh loop (and the connection it runs on); see `refresh`. */
  private refreshing: Promise<void> | null = null;
  private refreshingRpc: RpcClient | null = null;
  private refreshAgain = false;
  private refreshId = 0;
  /** `dashboard.get` requests issued / the newest one applied (stale responses are dropped). */
  private dashIssued = 0;
  private dashApplied = 0;

  constructor(
    record: HostRecord,
    private readonly o: ConnectionOptions,
  ) {
    this.state = {
      record,
      status: 'idle',
      error: null,
      closeCode: null,
      info: null,
      dashboard: null,
      cursor: null,
      lastOnlineAt: null,
      nextRetryAt: null,
    };
  }

  get id(): string {
    return this.state.record.host_id;
  }

  // ---- store interface ---------------------------------------------------------------------

  getSnapshot = (): HostState => this.state;

  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };

  onEvent(cb: (e: AppEvent) => void): () => void {
    this.eventListeners.add(cb);
    return () => this.eventListeners.delete(cb);
  }

  private set(patch: Partial<HostState>): void {
    this.state = { ...this.state, ...patch };
    for (const cb of [...this.listeners]) cb();
  }

  // ---- lifecycle ---------------------------------------------------------------------------

  start(): void {
    if (this.running) return;
    this.running = true;
    void this.connect();
  }

  stop(): void {
    this.running = false;
    this.generation++;
    this.clearTimers();
    this.rpc?.close();
    this.rpc = null;
    this.set({ status: 'idle', nextRetryAt: null });
  }

  /** Skip the backoff (e.g. app became visible, user tapped Retry). Also re-arms revoked hosts. */
  reconnectNow(): void {
    if (!this.running) return this.start();
    if (this.state.status === 'online' || this.state.status === 'connecting') return;
    this.attempts = 0;
    if (this.retryTimer !== null) this.o.platform.clock.clearTimeout(this.retryTimer);
    this.retryTimer = null;
    void this.connect();
  }

  setVisible(visible: boolean): void {
    const s = this.state.status;
    if (visible && s !== 'revoked' && s !== 'incompatible' && s !== 'expired') this.reconnectNow();
    if (this.rpc && this.state.status === 'online') {
      this.rpc.request('client.visibility', { visible }).catch(() => {});
    }
  }

  /** Typed app-API call. Mutating methods get an op_id automatically. */
  request<M extends AppMethod>(
    method: M,
    params: AppApi[M]['params'] & { op_id?: string },
    opts: Omit<RequestOptions, 'mutating'> = {},
  ): Promise<AppApi[M]['result']> {
    const rpc = this.rpc;
    if (!rpc || this.state.status !== 'online') return Promise.reject(new NotConnectedError(this.id));
    return rpc.request(method, params as Record<string, unknown>, { ...opts, mutating: MUTATING_METHODS.has(method) });
  }

  /**
   * Refetch the dashboard. Serialized per connection: a call while a fetch is in flight does not
   * start a second one; it marks a follow-up and resolves when a fetch issued after the call has
   * landed, so events arriving mid-request are never lost and responses never overtake. A failed
   * fetch with a follow-up queued still issues the follow-up (backed off); all callers sharing the
   * loop resolve or reject together.
   */
  refresh(): Promise<void> {
    const rpc = this.rpc;
    if (!rpc || !isDashboardHost(this.state.record)) return Promise.resolve();
    if (this.refreshing && this.refreshingRpc === rpc) {
      this.refreshAgain = true;
      return this.refreshing;
    }
    const id = ++this.refreshId;
    const loop = (async () => {
      let failures = 0;
      try {
        for (;;) {
          this.refreshAgain = false;
          try {
            await this.fetchDashboard(rpc);
            failures = 0;
          } catch (e) {
            // Callers that joined mid-flight wait for a fetch issued after their call: a failure
            // must not drop it. Issue the follow-up after a backoff; if that fails too (and
            // nobody asked again meanwhile), every waiter rejects with the same error.
            if (!this.refreshAgain || rpc !== this.rpc || failures >= REFRESH_MAX_RETRIES) throw e;
            failures++;
            await new Promise<void>((r) => this.o.platform.clock.setTimeout(r, Math.min(REFRESH_BACKOFF_MAX_MS, REFRESH_BACKOFF_MS * 2 ** (failures - 1))));
            if (rpc !== this.rpc) throw e;
            continue;
          }
          if (!this.refreshAgain || rpc !== this.rpc) return;
        }
      } finally {
        if (this.refreshId === id) {
          this.refreshing = null;
          this.refreshingRpc = null;
        }
      }
    })();
    this.refreshing = loop;
    this.refreshingRpc = rpc;
    return loop;
  }

  /**
   * One `dashboard.get`. Requests are numbered when issued; a response is applied only if nothing
   * issued later (a resync on the same connection) has been applied already, so an older snapshot
   * can never replace a newer one.
   */
  private async fetchDashboard(rpc: RpcClient): Promise<Dashboard | null> {
    const rev = this.dirtyRev;
    const seq = ++this.dashIssued;
    const d = normalizeDashboard(await rpc.request('dashboard.get', {}));
    if (rpc !== this.rpc || seq < this.dashApplied) return null;
    this.dashApplied = seq;
    this.cleanRev = rev;
    this.set({ dashboard: d });
    return d;
  }

  private get dashboardStale(): boolean {
    return this.dirtyRev !== this.cleanRev;
  }

  // ---- internals ---------------------------------------------------------------------------

  private async connect(): Promise<void> {
    const gen = ++this.generation;
    const { platform } = this.o;
    const { clock } = platform;
    const rec = this.state.record;
    if (hostExpired(rec, clock.now())) return this.expire();
    this.set({ status: 'connecting', nextRetryAt: null });

    let devicePrivate = this.o.devicePrivate;
    if (rec.key) {
      try {
        devicePrivate = await recordKey(platform.keystore, rec, this.o.devicePrivate);
      } catch (e) {
        if (gen !== this.generation) return;
        // Without its own key this record can never authenticate again: stop, as for a revoke.
        this.running = false;
        this.set({ status: 'revoked', error: (e as Error).message });
        return;
      }
      if (gen !== this.generation) return;
    }

    let channel: Channel;
    try {
      channel = await Channel.connect({
        socket: platform.connect(relayConnectUrl(rec.relay, rec.host_id)),
        hello: helloDevice(),
        hostKey: b64.decodeExact(rec.hk, 32),
        devicePrivate,
        clock,
        random: (n) => platform.random(n),
      });
    } catch (e) {
      if (gen === this.generation) this.onDisconnected(e);
      return;
    }
    if (gen !== this.generation) return channel.close();

    const rpc = new RpcClient(channel, { clock, random: (n) => platform.random(n) });
    this.rpc = rpc;
    rpc.on('event', (p) => this.onAppEvent(p as AppEvent));
    rpc.on('events.reset', () => void this.resync(rpc, false).catch(() => {}));
    rpc.on('device.revoked', () => {
      this.running = false;
      this.unauthorizedCount = UNAUTHORIZED_LIMIT;
      this.set({ status: 'revoked', error: 'device revoked by host' });
      rpc.close();
    });
    channel.onClose((err) => {
      if (gen === this.generation && this.rpc === rpc) this.onDisconnected(err);
    });

    try {
      const info = await rpc.request<HostInfo>('hello', {
        client: this.o.client.client,
        version: this.o.client.version,
        visible: platform.lifecycle.isVisible(),
      });
      if (gen !== this.generation) return;
      const updated = recordFromHello(this.state.record, info);
      if (updated) {
        this.set({ info, record: updated });
        void this.o.persist?.(updated).catch(() => {});
        if (this.expiryTimer !== null) clock.clearTimeout(this.expiryTimer);
        this.expiryTimer = null;
        if (hostExpired(updated, clock.now())) return this.expire();
      } else {
        this.set({ info });
      }
      // Handoff invitations only accept hello/ping/handoff.*: no dashboard, no events.
      if (isDashboardHost(this.state.record)) await this.resync(rpc, true);
      if (gen !== this.generation || rpc.closed) return;
      this.attempts = 0;
      this.unauthorizedCount = 0;
      this.set({ status: 'online', error: null, closeCode: null, lastOnlineAt: clock.now() });
      this.armExpiry();
    } catch (e) {
      // Setup failed on a live channel: drop it; onClose schedules the retry.
      if (gen === this.generation && !rpc.closed) {
        this.set({ error: (e as Error).message });
        rpc.close();
      }
    }
  }

  /**
   * Bring dashboard + event stream in line. With `resume` and a known cursor, first try to
   * replay from the gateway ring; `{reset:true}` (or no cursor) falls back to the snapshot barrier.
   */
  private async resync(rpc: RpcClient, resume: boolean): Promise<void> {
    if (resume && this.state.cursor !== null) {
      const r = await rpc.request<{ at: number; reset?: boolean }>('events.subscribe', { after: this.state.cursor });
      if (!r?.reset) {
        if (!this.state.dashboard || this.dashboardStale) await this.refresh();
        return;
      }
    }
    // A newer snapshot may have landed meanwhile (a refresh issued later): subscribe at that one.
    const at = (await this.fetchDashboard(rpc))?.at ?? (rpc === this.rpc ? this.state.dashboard?.at : undefined);
    if (at === undefined) return;
    this.set({ cursor: at });
    await rpc.request('events.subscribe', { after: at });
  }

  /** Go `expired` at the record's `until` instead of letting the gateway's refusals look like a revoke. */
  private armExpiry(): void {
    const until = this.state.record.until;
    if (until === undefined || this.expiryTimer !== null) return;
    const { clock } = this.o.platform;
    // Clamp: timers overflow past ~24.8 days; re-arm on the next connect if needed.
    const ms = Math.min(until * 1000 - clock.now(), 2 ** 31 - 1);
    this.expiryTimer = clock.setTimeout(() => {
      this.expiryTimer = null;
      if (hostExpired(this.state.record, clock.now())) this.expire();
      else this.armExpiry();
    }, Math.max(0, ms));
  }

  private expire(): void {
    this.running = false;
    this.generation++;
    this.clearTimers();
    const rpc = this.rpc;
    this.rpc = null;
    rpc?.close();
    this.set({ status: 'expired', error: null, dashboard: null, nextRetryAt: null });
  }

  private onAppEvent(e: AppEvent): void {
    if (typeof e?.seq !== 'number') return;
    if (this.state.cursor !== null && e.seq <= this.state.cursor) return; // at-least-once: dedupe
    this.set({ cursor: e.seq });
    for (const cb of [...this.eventListeners]) cb(e);
    if (REFRESH_PREFIXES.some((p) => e.type.startsWith(p))) {
      this.dirtyRev++;
      this.scheduleRefresh();
    }
  }

  private scheduleRefresh(): void {
    if (this.refreshTimer !== null) return;
    const { clock } = this.o.platform;
    this.refreshTimer = clock.setTimeout(() => {
      this.refreshTimer = null;
      this.refresh().catch(() => {});
    }, this.o.refreshDebounceMs ?? 250);
  }

  private onDisconnected(err: unknown): void {
    this.rpc = null;
    if (this.refreshTimer !== null) this.o.platform.clock.clearTimeout(this.refreshTimer);
    this.refreshTimer = null;
    if (!this.running) return;
    if (hostExpired(this.state.record, this.o.platform.clock.now())) return this.expire();
    const ce = err instanceof ChannelError ? err : null;
    const closeCode = ce?.closeCode ?? null;
    if (ce?.code === 'unauthorized') {
      // Unauthenticated: a relay can forge it. Only the third in a row counts as revoked (§4.4).
      this.unauthorizedCount++;
      if (this.unauthorizedCount >= UNAUTHORIZED_LIMIT) {
        this.running = false;
        this.set({ status: 'revoked', error: 'host refused this device repeatedly', closeCode });
        return;
      }
      this.scheduleRetry('unauthorized', 'host refused this device (maybe revoked)', closeCode);
      return;
    }
    if (this.state.status === 'revoked') return;
    if (ce?.code === 'unsupported_version') {
      this.running = false;
      this.set({ status: 'incompatible', error: ce.message, closeCode });
      return;
    }
    this.scheduleRetry('offline', err ? (err as Error).message : (this.state.error ?? 'disconnected'), closeCode);
  }

  private scheduleRetry(status: HostStatus, error: string, closeCode: number | null): void {
    const { clock, random } = this.o.platform;
    const min = this.o.backoffMinMs ?? 1_000;
    const max = this.o.backoffMaxMs ?? 30_000;
    const base = Math.min(max, min * 2 ** this.attempts++);
    // Equal jitter: [base/2, base), so retries from many devices spread out but never hammer.
    const r = new DataView(random(4).buffer).getUint32(0) / 2 ** 32;
    const delay = Math.round(base / 2 + (r * base) / 2);
    const gen = this.generation;
    this.retryTimer = clock.setTimeout(() => {
      this.retryTimer = null;
      if (this.running && gen === this.generation) void this.connect();
    }, delay);
    this.set({ status, error, closeCode, nextRetryAt: clock.now() + delay });
  }

  private clearTimers(): void {
    const { clock } = this.o.platform;
    if (this.retryTimer !== null) clock.clearTimeout(this.retryTimer);
    if (this.refreshTimer !== null) clock.clearTimeout(this.refreshTimer);
    if (this.expiryTimer !== null) clock.clearTimeout(this.expiryTimer);
    this.retryTimer = this.refreshTimer = this.expiryTimer = null;
  }
}

/**
 * What screens need from one host connection. `HostConnection` implements it; shells that run
 * the connections elsewhere (Electron: the main process) hand the UI a proxy with the same shape.
 */
export interface HostConnectionApi {
  readonly id: string;
  getSnapshot(): HostState;
  subscribe(cb: () => void): () => void;
  request<M extends AppMethod>(
    method: M,
    params: AppApi[M]['params'] & { op_id?: string },
    opts?: Omit<RequestOptions, 'mutating'>,
  ): Promise<AppApi[M]['result']>;
  refresh(): Promise<void>;
  reconnectNow(): void;
}

/** What screens need from the set of paired hosts (`HostManager` or a proxy of it). */
export interface HostManagerApi {
  getSnapshot(): readonly HostState[];
  subscribe(cb: () => void): () => void;
  get(hostId: string): HostConnectionApi | undefined;
  connections(): HostConnectionApi[];
  remove(hostId: string): Promise<void>;
  stop(): void;
  /**
   * Live app events of one host, in order (deduped by seq). Survives reconnects and re-pairing of
   * the same host id. Shells that run connections elsewhere may forward only the event types the
   * UI needs (agent.*, interaction.*, task.*, preview.*, tab.*, pane.*, notification.created).
   */
  subscribeEvents(hostId: string, cb: (e: AppEvent) => void): () => void;
}

export interface HostManagerOptions extends ConnectionOptions {
  store: HostStore;
}

/** All paired hosts. `getSnapshot` returns a stable array until something changes. */
export class HostManager implements HostManagerApi {
  private readonly conns = new Map<string, HostConnection>();
  private readonly listeners = new Set<() => void>();
  private readonly unsubs = new Map<string, () => void>();
  private readonly eventSubs = new Map<string, Set<(e: AppEvent) => void>>();
  private readonly anyEventSubs = new Set<(hostId: string, e: AppEvent) => void>();
  private snapshot: readonly HostState[] = [];
  private lifecycleOff: (() => void)[] = [];

  constructor(private readonly o: HostManagerOptions) {}

  async start(): Promise<void> {
    for (const r of await this.o.store.list()) this.attach(r);
    const lc = this.o.platform.lifecycle;
    this.lifecycleOff = [
      lc.onVisible(() => this.conns.forEach((c) => c.setVisible(true))),
      lc.onHidden(() => this.conns.forEach((c) => c.setVisible(false))),
    ];
    this.conns.forEach((c) => c.start());
  }

  stop(): void {
    this.lifecycleOff.forEach((f) => f());
    this.lifecycleOff = [];
    this.conns.forEach((c) => c.stop());
  }

  /** Persist and connect a newly paired host (replaces an existing record with the same id). */
  async add(record: HostRecord): Promise<HostConnection> {
    await this.o.store.put(record);
    this.detach(record.host_id);
    const c = this.attach(record);
    c.start();
    return c;
  }

  async remove(hostId: string): Promise<void> {
    this.detach(hostId);
    await this.o.store.remove(hostId);
  }

  get(hostId: string): HostConnection | undefined {
    return this.conns.get(hostId);
  }

  connections(): HostConnection[] {
    return [...this.conns.values()];
  }

  getSnapshot = (): readonly HostState[] => this.snapshot;

  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };

  subscribeEvents(hostId: string, cb: (e: AppEvent) => void): () => void {
    let set = this.eventSubs.get(hostId);
    if (!set) this.eventSubs.set(hostId, (set = new Set()));
    set.add(cb);
    return () => {
      set.delete(cb);
      if (!set.size && this.eventSubs.get(hostId) === set) this.eventSubs.delete(hostId);
    };
  }

  /** Every host's events (the desktop engine forwards a filtered subset to its windows). */
  onAnyEvent(cb: (hostId: string, e: AppEvent) => void): () => void {
    this.anyEventSubs.add(cb);
    return () => this.anyEventSubs.delete(cb);
  }

  private dispatchEvent(hostId: string, e: AppEvent): void {
    for (const cb of [...(this.eventSubs.get(hostId) ?? [])]) {
      try {
        cb(e);
      } catch {
        /* a listener's failure must not break the others */
      }
    }
    for (const cb of [...this.anyEventSubs]) {
      try {
        cb(hostId, e);
      } catch {
        /* ignore */
      }
    }
  }

  private attach(record: HostRecord): HostConnection {
    const c = new HostConnection(record, { ...this.o, persist: (r) => this.o.store.put(r) });
    const id = record.host_id;
    this.conns.set(id, c);
    const offState = c.subscribe(() => this.changed());
    const offEvents = c.onEvent((e) => this.dispatchEvent(id, e));
    this.unsubs.set(id, () => (offState(), offEvents()));
    this.changed();
    return c;
  }

  private detach(hostId: string): void {
    const c = this.conns.get(hostId);
    if (!c) return;
    c.stop();
    this.unsubs.get(hostId)?.();
    this.unsubs.delete(hostId);
    this.conns.delete(hostId);
    this.changed();
  }

  private changed(): void {
    this.snapshot = [...this.conns.values()].map((c) => c.getSnapshot());
    for (const cb of [...this.listeners]) cb();
  }
}
