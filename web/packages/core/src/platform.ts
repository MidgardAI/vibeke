// Platform capabilities injected into core (spec 16 §9.3). Shells (PWA, Electron, tests)
// implement these; core never touches DOM, IndexedDB, WebSocket or timers directly.

/** Opaque timer handle. */
export type TimerHandle = unknown;

/** Time and timers. Injected so tests can drive deadlines deterministically. */
export interface Clock {
  /** Milliseconds since the Unix epoch. */
  now(): number;
  setTimeout(fn: () => void, ms: number): TimerHandle;
  clearTimeout(handle: TimerHandle): void;
}

export const systemClock: Clock = {
  now: () => Date.now(),
  setTimeout: (fn, ms) => globalThis.setTimeout(fn, ms),
  clearTimeout: (h) => globalThis.clearTimeout(h as ReturnType<typeof globalThis.setTimeout>),
};

export type SocketState = 'connecting' | 'open' | 'closed';

/**
 * A message-oriented duplex connection (a WebSocket, an Electron IPC bridge, or an in-memory pipe).
 * Text messages are strings, binary messages are Uint8Array (type is preserved end to end).
 * Shells map socket errors to `onclose`.
 */
export interface Socket {
  readonly state: SocketState;
  send(data: string | Uint8Array): void;
  close(code?: number, reason?: string): void;
  onopen: (() => void) | null;
  onmessage: ((data: string | Uint8Array) => void) | null;
  onclose: ((code: number, reason: string) => void) | null;
}

/** Small persistent secret store (IndexedDB in the PWA, safeStorage file in Electron). */
export interface KeyStore {
  get(name: string): Promise<Uint8Array | null>;
  set(name: string, value: Uint8Array): Promise<void>;
  delete(name: string): Promise<void>;
  /**
   * Optional atomic get-or-create: return the stored value if `valid`, else store `make()` and
   * return it, as one step that no other tab/process can interleave with (e.g. one IndexedDB
   * readwrite transaction under a Web Lock). Without it core falls back to get + set,
   * serialized in-process only.
   */
  getOrCreate?(name: string, make: () => Uint8Array, valid: (v: Uint8Array) => boolean): Promise<Uint8Array>;
}

const inflight = new WeakMap<KeyStore, Map<string, Promise<Uint8Array>>>();

/**
 * Get-or-create a key exactly once even when called concurrently (two startups racing): calls in
 * this process share one in-flight promise, and the store's atomic `getOrCreate` (when present)
 * covers other tabs.
 */
export function getOrCreateKey(
  store: KeyStore,
  name: string,
  make: () => Uint8Array,
  valid: (v: Uint8Array) => boolean,
): Promise<Uint8Array> {
  let m = inflight.get(store);
  if (!m) inflight.set(store, (m = new Map()));
  const running = m.get(name);
  if (running) return running;
  const p = (async () => {
    if (store.getOrCreate) return store.getOrCreate(name, make, valid);
    const existing = await store.get(name);
    if (existing && valid(existing)) return existing;
    const k = make();
    await store.set(name, k);
    return k;
  })();
  const map = m;
  map.set(name, p);
  const done = () => {
    if (map.get(name) === p) map.delete(name);
  };
  p.then(done, done);
  return p;
}

export interface Lifecycle {
  /** Register a callback; returns an unsubscribe function. */
  onVisible(cb: () => void): () => void;
  onHidden(cb: () => void): () => void;
  /** Current visibility. */
  isVisible(): boolean;
}

export interface LocalNotification {
  tag: string;
  title: string;
  body: string;
  /** Deep link opened on tap, e.g. `#/i/<host>/<interaction>`. */
  url?: string;
  renotify?: boolean;
}

export interface PushSupport {
  /** Current subscription (Web Push JSON form) or null. */
  getSubscription(): Promise<PushSubscriptionInfo | null>;
  /** Subscribe with the device's VAPID public key (uncompressed P-256, base64url). */
  subscribe(vapidPublic: string): Promise<PushSubscriptionInfo>;
  unsubscribe(): Promise<void>;
  /**
   * The browser accepts a push that shows nothing (Chrome, Firefox). Hosts then send `clear`
   * pushes that only close notifications. False where the browser revokes push for silent pushes (Apple WebKit).
   */
  supportsClear?: boolean;
}

export interface PushSubscriptionInfo {
  endpoint: string;
  expirationTime?: number | null;
  keys: { p256dh: string; auth: string };
}

export interface Platform {
  keystore: KeyStore;
  /** Open a socket; the returned socket starts in `connecting` (or already `open`). */
  connect(url: string): Socket;
  clock: Clock;
  /** Cryptographically secure random bytes. */
  random(n: number): Uint8Array;
  lifecycle: Lifecycle;
  /** Short platform label sent at pairing, e.g. `iOS`, `Android`, `macOS`. */
  platformName: string;
  notify?(n: LocalNotification): void;
  push?: PushSupport;
}
