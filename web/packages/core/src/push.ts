// Device-owned Web Push (spec 16 §8.1, §8.3): one P-256 VAPID key pair per device, one browser
// subscription signed by it, and `push.subscribe {subscription, vapid_private}` sent to EVERY
// paired host. The browser specifics (PushManager, permission prompts) live behind
// `PushSupport`; this module owns the key and keeps all hosts in sync.

import { p256 } from '@noble/curves/nist.js';
import * as b64 from './b64';
import { isDashboardHost, type HostManager } from './hosts';
import { getOrCreateKey, type KeyStore, type PushSubscriptionInfo, type PushSupport } from './platform';

export const VAPID_KEY_NAME = 'device_vapid_private';
const SENT_KEY_NAME = 'push_sent';

export interface VapidKeys {
  /** Raw 32-byte P-256 scalar. */
  privateKey: Uint8Array;
  /** Uncompressed point (65 bytes), the `applicationServerKey`. */
  publicKey: Uint8Array;
}

export function vapidFromPrivate(privateKey: Uint8Array): VapidKeys {
  return { privateKey, publicKey: p256.getPublicKey(privateKey, false) };
}

export function generateVapid(random: (n: number) => Uint8Array): VapidKeys {
  for (;;) {
    const k = random(32);
    if (p256.utils.isValidSecretKey(k)) return vapidFromPrivate(k);
  }
}

export async function loadOrCreateVapid(store: KeyStore, random: (n: number) => Uint8Array): Promise<VapidKeys> {
  const k = await getOrCreateKey(
    store,
    VAPID_KEY_NAME,
    () => generateVapid(random).privateKey,
    (v) => v.length === 32 && p256.utils.isValidSecretKey(v),
  );
  return vapidFromPrivate(k);
}

const enc = new TextEncoder();
const dec = new TextDecoder();

/** Marker of what a host last received: endpoint + VAPID public key. */
const marker = (sub: PushSubscriptionInfo, keys: VapidKeys): string => `${sub.endpoint}|${b64.encode(keys.publicKey)}`;

export type PushSyncState = 'unsupported' | 'off' | 'on' | 'error';

export interface PushSyncOptions {
  manager: HostManager;
  push: PushSupport | undefined;
  keystore: KeyStore;
  random: (n: number) => Uint8Array;
}

/**
 * Keeps every online host's push registration in line with the device subscription.
 * - `start()` reads the current subscription (spec §8.3: refreshed on every app start) and
 *   re-sends it to any host whose marker differs (new host, changed endpoint, rotated key).
 * - `enable()` must be called from a user gesture (iOS); it subscribes and sends to all hosts.
 * - `disable()` unsubscribes the browser and tells every reachable host.
 * - `rotate()` (after forgetting a host, §8.1) makes a new key, resubscribes, re-sends.
 */
export class PushSync {
  private sub: PushSubscriptionInfo | null = null;
  private keys: VapidKeys | null = null;
  private sent: Record<string, string> = {};
  private inflight = new Set<string>();
  private off: (() => void) | null = null;
  private listeners = new Set<() => void>();
  private _state: PushSyncState;
  private _error: string | null = null;

  constructor(private readonly o: PushSyncOptions) {
    this._state = o.push ? 'off' : 'unsupported';
  }

  get state(): PushSyncState {
    return this._state;
  }
  get error(): string | null {
    return this._error;
  }
  get subscription(): PushSubscriptionInfo | null {
    return this.sub;
  }

  getSnapshot = (): PushSyncState => this._state;
  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };
  private emit(state: PushSyncState, error: string | null = null): void {
    this._state = state;
    this._error = error;
    for (const cb of [...this.listeners]) cb();
  }

  async start(): Promise<void> {
    if (!this.o.push) return;
    try {
      this.keys = await loadOrCreateVapid(this.o.keystore, this.o.random);
      this.sent = await this.loadSent();
      this.sub = await this.o.push.getSubscription();
    } catch (e) {
      this.emit('error', (e as Error).message);
      return;
    }
    this.emit(this.sub ? 'on' : 'off');
    this.off?.();
    this.off = this.o.manager.subscribe(() => void this.syncAll());
    await this.syncAll();
  }

  stop(): void {
    this.off?.();
    this.off = null;
  }

  async enable(): Promise<void> {
    const push = this.o.push;
    if (!push) throw new Error('push is not supported here');
    this.keys ??= await loadOrCreateVapid(this.o.keystore, this.o.random);
    try {
      this.sub = await push.subscribe(b64.encode(this.keys.publicKey));
    } catch (e) {
      this.emit('error', (e as Error).message);
      throw e;
    }
    this.emit('on');
    await this.syncAll(true);
  }

  async disable(): Promise<void> {
    const push = this.o.push;
    const endpoint = this.sub?.endpoint;
    this.sub = null;
    await Promise.all(
      this.o.manager.connections().map((c) =>
        c.getSnapshot().status === 'online' && isDashboardHost(c.getSnapshot().record)
          ? c.request('push.unsubscribe', endpoint ? { endpoint } : {}).catch(() => {})
          : undefined,
      ),
    );
    this.sent = {};
    await this.saveSent();
    await push?.unsubscribe().catch(() => {});
    this.emit('off');
  }

  /** New key pair + resubscribe + re-send to remaining hosts (after a host is forgotten). */
  async rotate(): Promise<void> {
    const push = this.o.push;
    if (!push) return;
    const keys = generateVapid(this.o.random);
    await this.o.keystore.set(VAPID_KEY_NAME, keys.privateKey);
    this.keys = keys;
    if (!this.sub) return;
    await push.unsubscribe().catch(() => {});
    try {
      this.sub = await push.subscribe(b64.encode(keys.publicKey));
    } catch (e) {
      this.sub = null;
      this.emit('error', (e as Error).message);
      return;
    }
    await this.syncAll(true);
  }

  /** Forget the marker of a removed host. */
  async forgetHost(hostId: string): Promise<void> {
    delete this.sent[hostId];
    await this.saveSent();
  }

  /** Send to every online host whose marker is out of date (or all, with `force`). */
  async syncAll(force = false): Promise<void> {
    const sub = this.sub;
    const keys = this.keys;
    if (!sub || !keys) return;
    const want = marker(sub, keys);
    const jobs: Promise<void>[] = [];
    for (const c of this.o.manager.connections()) {
      const st = c.getSnapshot();
      // Handoff invitations cannot (and need not) push to this device.
      if (st.status !== 'online' || !isDashboardHost(st.record) || this.inflight.has(c.id)) continue;
      if (!force && this.sent[c.id] === want) continue;
      this.inflight.add(c.id);
      jobs.push(
        c
          .request('push.subscribe', {
            subscription: { endpoint: sub.endpoint, keys: { p256dh: sub.keys.p256dh, auth: sub.keys.auth } },
            vapid_private: b64.encode(keys.privateKey),
          })
          .then(
            () => {
              this.sent[c.id] = want;
            },
            () => {
              // Leave the marker stale; the next online transition retries.
            },
          )
          .finally(() => this.inflight.delete(c.id)),
      );
    }
    if (jobs.length === 0) return;
    await Promise.all(jobs);
    await this.saveSent();
  }

  private async loadSent(): Promise<Record<string, string>> {
    const raw = await this.o.keystore.get(SENT_KEY_NAME);
    if (!raw) return {};
    try {
      const v = JSON.parse(dec.decode(raw)) as unknown;
      return v && typeof v === 'object' ? (v as Record<string, string>) : {};
    } catch {
      return {};
    }
  }

  private async saveSent(): Promise<void> {
    await this.o.keystore.set(SENT_KEY_NAME, enc.encode(JSON.stringify(this.sent)));
  }
}
