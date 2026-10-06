// Renderer-side proxy of the main process's HostManager (spec 16 §16.1). Host state arrives as
// patches; requests go through the bridge and errors are rebuilt into core's classes so the UI's
// error handling (stale / unknown outcome / offline) works unchanged.

import {
  NotConnectedError,
  OutcomeUnknownError,
  PairingError,
  ChannelError,
  RpcError,
  type AppApi,
  type AppEvent,
  type AppMethod,
  type HostConnectionApi,
  type HostManagerApi,
  type HostState,
  type RequestOptions,
} from '@vibeke/core';
import { EVENT, INVOKE, type Bridge, type HostsPatch, type WireError, type WireResult } from '../shared/contract';
import { parseHostEvent } from '../shared/host-events';

export function fromWire(e: WireError): Error {
  switch (e.type) {
    case 'rpc':
      return new RpcError(e.method, { code: e.code, message: e.message, data: e.data as never });
    case 'unknown':
      return new OutcomeUnknownError(e.method, e.mutating, e.opId, e.reason);
    case 'not_connected':
      return new NotConnectedError(e.hostId);
    case 'pairing': {
      const cause = e.channelCode ? new ChannelError(e.channelCode as never, e.message, { closeCode: e.closeCode }) : undefined;
      return new PairingError(e.code as never, e.message, cause);
    }
    default:
      return Object.assign(new Error(e.message), e.code ? { code: e.code } : {});
  }
}

export async function call<T>(bridge: Bridge, channel: (typeof INVOKE)[keyof typeof INVOKE], ...args: unknown[]): Promise<T> {
  const r = (await bridge.invoke(channel, ...args)) as WireResult<T>;
  if (!r || typeof r !== 'object' || !('ok' in r)) throw new Error('bad IPC reply');
  if (r.ok) return r.value;
  throw fromWire(r.error);
}

class RemoteConnection implements HostConnectionApi {
  private listeners = new Set<() => void>();
  constructor(
    private state: HostState,
    private readonly bridge: Bridge,
  ) {}

  get id(): string {
    return this.state.record.host_id;
  }
  getSnapshot = (): HostState => this.state;
  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };
  set(s: HostState): void {
    this.state = s;
    for (const cb of [...this.listeners]) cb();
  }

  request<M extends AppMethod>(method: M, params: AppApi[M]['params'] & { op_id?: string }, opts: Omit<RequestOptions, 'mutating'> = {}): Promise<AppApi[M]['result']> {
    const o = opts.timeoutMs !== undefined ? { timeoutMs: opts.timeoutMs } : undefined;
    return call(this.bridge, INVOKE.request, this.id, method, params, o);
  }
  async refresh(): Promise<void> {
    await call(this.bridge, INVOKE.refresh, this.id);
  }
  reconnectNow(): void {
    void call(this.bridge, INVOKE.reconnect, this.id).catch(() => {});
  }
}

export class RemoteManager implements HostManagerApi {
  private conns = new Map<string, RemoteConnection>();
  private order: string[] = [];
  private snapshot: readonly HostState[] = [];
  private listeners = new Set<() => void>();
  private off: (() => void) | null = null;
  /** Version of the newest state applied; null until the snapshot arrived. */
  private version: number | null = null;
  /** Patches that arrived before the snapshot (subscribed first, so none are lost). */
  private early: HostsPatch[] = [];
  private eventSubs = new Map<string, Set<(e: AppEvent) => void>>();
  private offEvents: (() => void) | null = null;

  /** Subscribes to patches immediately; call `attach` with the snapshot fetched afterwards. */
  constructor(private readonly bridge: Bridge) {
    this.off = this.bridge.on(EVENT.hosts, (p) => this.receive(p as HostsPatch));
  }

  /**
   * Live events of one host, forwarded by main (only the types the UI needs). Main sends them
   * only while this window has at least one listener for the host.
   */
  subscribeEvents(hostId: string, cb: (e: AppEvent) => void): () => void {
    this.offEvents ??= this.bridge.on(EVENT.hostEvent, (p) => this.onHostEvent(p));
    let set = this.eventSubs.get(hostId);
    if (!set) {
      this.eventSubs.set(hostId, (set = new Set()));
      void call(this.bridge, INVOKE.hostEvents, hostId, true).catch(() => {});
    }
    set.add(cb);
    return () => {
      if (!set.delete(cb) || set.size || this.eventSubs.get(hostId) !== set) return;
      this.eventSubs.delete(hostId);
      void call(this.bridge, INVOKE.hostEvents, hostId, false).catch(() => {});
    };
  }

  private onHostEvent(p: unknown): void {
    const ev = parseHostEvent(p);
    if (!ev) return;
    for (const cb of [...(this.eventSubs.get(ev.hostId) ?? [])]) {
      try {
        cb(ev.event);
      } catch {
        /* one listener's failure must not starve the others */
      }
    }
  }

  /** Seed with the snapshot (at `version`), then replay patches newer than it. */
  attach(initial: HostState[], version: number): void {
    this.version = version;
    this.apply({ version, order: initial.map((s) => s.record.host_id), changed: initial });
    const early = this.early;
    this.early = [];
    for (const p of early) this.receive(p);
  }

  private receive(p: HostsPatch): void {
    if (!p || typeof p.version !== 'number') return;
    if (this.version === null) return void this.early.push(p);
    if (p.version < this.version) return; // older than what we show (a full catch-up equals it)
    this.version = p.version;
    this.apply(p);
  }

  private apply(p: HostsPatch): void {
    for (const s of p.changed) {
      const id = s.record.host_id;
      const c = this.conns.get(id);
      if (c) c.set(s);
      else this.conns.set(id, new RemoteConnection(s, this.bridge));
    }
    for (const id of [...this.conns.keys()]) if (!p.order.includes(id)) this.conns.delete(id);
    this.order = p.order.filter((id) => this.conns.has(id));
    this.snapshot = this.order.map((id) => this.conns.get(id)!.getSnapshot());
    for (const cb of [...this.listeners]) cb();
  }

  getSnapshot = (): readonly HostState[] => this.snapshot;
  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };
  get(hostId: string): HostConnectionApi | undefined {
    return this.conns.get(hostId);
  }
  connections(): HostConnectionApi[] {
    return this.order.map((id) => this.conns.get(id)!).filter(Boolean);
  }
  async remove(hostId: string): Promise<void> {
    await call(this.bridge, INVOKE.remove, hostId);
  }
  /** Windows come and go; the connections live on in the main process. */
  stop(): void {
    this.off?.();
    this.off = null;
    this.offEvents?.();
    this.offEvents = null;
  }
}
