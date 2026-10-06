// The connection engine (spec 16 §16.1): core's HostManager running in the main process with
// the device key from the vault, so connections survive closed windows and one connection per
// host serves every window. Renderers see host state through `HostsPatch` updates and call the
// app API through `request` (validated in ipc.ts). Electron-free (injected platform) for tests.

import {
  ChannelError,
  HostManager,
  NotConnectedError,
  OutcomeUnknownError,
  PairingError,
  RpcError,
  fingerprint,
  loadOrCreateDeviceKey,
  pair,
  x25519Public,
  type HostRecord,
  type HostState,
  type HostStore,
  type PairingLink,
  type Platform,
} from '@vibeke/core';
import type { HostsPatch, RendererMethod, WireError } from '../shared/contract';

export function toWire(e: unknown): WireError {
  if (e instanceof RpcError) {
    const prefix = `${e.method}: `;
    return { type: 'rpc', method: e.method, code: e.code, message: e.message.startsWith(prefix) ? e.message.slice(prefix.length) : e.message, data: e.data as Record<string, unknown> | undefined };
  }
  if (e instanceof OutcomeUnknownError) return { type: 'unknown', method: e.method, mutating: e.mutating, opId: e.opId, reason: e.reason };
  if (e instanceof NotConnectedError) return { type: 'not_connected', hostId: e.hostId };
  if (e instanceof PairingError) {
    const c = e.cause instanceof ChannelError ? e.cause : null;
    return { type: 'pairing', code: e.code, message: e.message, channelCode: c?.code, closeCode: c?.closeCode };
  }
  const err = e as { message?: string; code?: unknown };
  return { type: 'error', message: err?.message ?? String(e), code: typeof err?.code === 'string' ? err.code : undefined };
}

export interface EngineOptions {
  platform: Platform;
  hostStore: HostStore;
  client: { client: string; version: string };
}

export class Engine {
  manager: HostManager | null = null;
  private devicePrivate: Uint8Array | null = null;
  private starting: Promise<void> | null = null;
  private last = new Map<string, HostState>();
  private listeners = new Set<(p: HostsPatch) => void>();
  /** Bumped per published patch; snapshots carry it so renderers can drop older patches. */
  version = 0;

  constructor(private readonly o: EngineOptions) {}

  get fingerprint(): string {
    if (!this.devicePrivate) throw new Error('engine not started');
    return fingerprint(x25519Public(this.devicePrivate));
  }

  /** Idempotent: every window's `engine.start` shares one startup. */
  start(): Promise<void> {
    this.starting ??= (async () => {
      const p = this.o.platform;
      this.devicePrivate = await loadOrCreateDeviceKey(p.keystore, (n) => p.random(n));
      const m = new HostManager({ platform: p, store: this.o.hostStore, devicePrivate: this.devicePrivate, client: this.o.client });
      this.manager = m;
      m.subscribe(() => this.publish());
      await m.start();
      this.publish();
    })();
    // A failed start (keyring missing) can be retried after the user fixes it.
    this.starting.catch(() => (this.starting = null));
    return this.starting;
  }

  stop(): void {
    this.manager?.stop();
  }

  snapshot(): HostState[] {
    return [...(this.manager?.getSnapshot() ?? [])];
  }

  /** Everything, as one patch at the current version (a window catching up after being hidden). */
  fullPatch(): HostsPatch {
    const s = this.snapshot();
    return { version: this.version, order: s.map((x) => x.record.host_id), changed: s };
  }

  /** Changed host states since the last publish (states are immutable; identity = unchanged). */
  onPatch(cb: (p: HostsPatch) => void): () => void {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  }

  private publish(): void {
    const states = this.manager?.getSnapshot() ?? [];
    const changed: HostState[] = [];
    const seen = new Set<string>();
    for (const s of states) {
      seen.add(s.record.host_id);
      if (this.last.get(s.record.host_id) !== s) changed.push(s);
      this.last.set(s.record.host_id, s);
    }
    let removed = false;
    for (const id of [...this.last.keys()]) if (!seen.has(id)) (this.last.delete(id), (removed = true));
    if (!changed.length && !removed) return;
    const patch: HostsPatch = { version: ++this.version, order: states.map((s) => s.record.host_id), changed };
    for (const cb of [...this.listeners]) cb(patch);
  }

  async request(hostId: string, method: RendererMethod, params: Record<string, unknown>, opts: { timeoutMs?: number }): Promise<unknown> {
    const c = this.manager?.get(hostId);
    if (!c) throw new NotConnectedError(hostId);
    return c.request(method, params as never, opts);
  }

  async refresh(hostId: string): Promise<void> {
    await this.manager?.get(hostId)?.refresh();
  }

  reconnect(hostId: string | null): void {
    if (hostId === null) this.manager?.connections().forEach((c) => c.reconnectNow());
    else this.manager?.get(hostId)?.reconnectNow();
  }

  async remove(hostId: string): Promise<void> {
    await this.manager?.remove(hostId);
  }

  async pair(link: PairingLink, deviceName: string, onPending: (fp: string) => void): Promise<HostRecord> {
    await this.start();
    if (!this.devicePrivate || !this.manager) throw new Error('engine not started');
    const record = await pair({ link, platform: this.o.platform, devicePrivate: this.devicePrivate, deviceName, onPending });
    await this.manager.add(record);
    return record;
  }
}
