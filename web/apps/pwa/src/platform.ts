// The PWA's UiPlatform: IndexedDB keys and hosts, WebSocket transport, Web Push through the
// service worker, install prompt capture, Web Speech / MediaRecorder, haptics.

import { b64, systemClock, type Dashboard, type HostRecord, type HostStore, type KeyStore, type Lifecycle, type PushSubscriptionInfo, type PushSupport } from '@vibeke/core';
import type { InstallCapability, NotificationsCapability, PermissionState, SpeechCapability, UiPlatform } from '@vibeke/ui';
import { idbAll, idbDelete, idbGet, idbGetOrCreate, idbSet, persist } from './idb';
import { connectWebSocket } from './ws-socket';
import { detectPlatform } from './detect';

declare const __BUILD_HASH__: string;
declare const __APP_VERSION__: string;

const asBytes = (v: unknown): Uint8Array | null =>
  v instanceof Uint8Array ? v : v instanceof ArrayBuffer ? new Uint8Array(v) : null;

type Locks = { request<T>(name: string, cb: () => Promise<T>): Promise<T> };

const keystore: KeyStore = {
  async get(name) {
    return asBytes(await idbGet<unknown>('keys', name));
  },
  set: (name, value) => idbSet('keys', name, new Uint8Array(value)),
  delete: (name) => idbDelete('keys', name),
  getOrCreate(name, make, valid) {
    const run = () =>
      idbGetOrCreate<Uint8Array>(
        'keys',
        name,
        (v): v is Uint8Array => {
          const b = asBytes(v);
          return b !== null && valid(b);
        },
        () => new Uint8Array(make()),
      ).then((v) => asBytes(v)!);
    // The transaction alone is atomic; a Web Lock additionally keeps tabs from even racing to it.
    const locks = (navigator as Navigator & { locks?: Locks }).locks;
    return locks ? locks.request(`vibeke-key:${name}`, run) : run();
  },
};

const hostStore: HostStore = {
  list: () => idbAll<HostRecord>('hosts'),
  put: (r) => idbSet('hosts', r.host_id, r),
  remove: (id) => idbDelete('hosts', id),
};

const lifecycle: Lifecycle = {
  onVisible(cb) {
    const f = () => document.visibilityState === 'visible' && cb();
    document.addEventListener('visibilitychange', f);
    window.addEventListener('pageshow', f);
    return () => {
      document.removeEventListener('visibilitychange', f);
      window.removeEventListener('pageshow', f);
    };
  },
  onHidden(cb) {
    const f = () => document.visibilityState === 'hidden' && cb();
    document.addEventListener('visibilitychange', f);
    return () => document.removeEventListener('visibilitychange', f);
  },
  isVisible: () => document.visibilityState === 'visible',
};

const kv = {
  get(k: string) {
    try {
      return localStorage.getItem(k);
    } catch {
      return null;
    }
  },
  set(k: string, v: string) {
    try {
      localStorage.setItem(k, v);
    } catch {
      /* private mode */
    }
  },
  remove(k: string) {
    try {
      localStorage.removeItem(k);
    } catch {
      /* ignore */
    }
  },
  // Another tab changed it (prefs stay in sync across tabs).
  watch(k: string, cb: (v: string | null) => void) {
    const f = (e: StorageEvent) => e.key === k && cb(e.newValue);
    window.addEventListener('storage', f);
    return () => window.removeEventListener('storage', f);
  },
};

const standalone = (): boolean =>
  (typeof matchMedia === 'function' && matchMedia('(display-mode: standalone)').matches) ||
  (navigator as Navigator & { standalone?: boolean }).standalone === true;

const swReady = (): Promise<ServiceWorkerRegistration> | null => ('serviceWorker' in navigator ? navigator.serviceWorker.ready : null);

function subInfo(s: PushSubscription): PushSubscriptionInfo {
  const j = s.toJSON();
  return { endpoint: s.endpoint, expirationTime: s.expirationTime, keys: { p256dh: j.keys?.p256dh ?? '', auth: j.keys?.auth ?? '' } };
}

function pushSupport(): PushSupport | undefined {
  if (!('serviceWorker' in navigator) || !('PushManager' in window)) return undefined;
  return {
    async getSubscription() {
      const reg = await swReady()!;
      const s = await reg.pushManager.getSubscription();
      return s ? subInfo(s) : null;
    },
    async subscribe(vapidPublic) {
      const reg = await swReady()!;
      const key = b64.decode(vapidPublic);
      const existing = await reg.pushManager.getSubscription();
      if (existing) {
        // A subscription is bound to one applicationServerKey; replace it if the key changed.
        const cur = existing.options.applicationServerKey;
        const same = cur && b64.encode(new Uint8Array(cur)) === vapidPublic;
        if (same) return subInfo(existing);
        await existing.unsubscribe();
      }
      const s = await reg.pushManager.subscribe({ userVisibleOnly: true, applicationServerKey: key as BufferSource });
      return subInfo(s);
    },
    async unsubscribe() {
      const reg = await swReady()!;
      await (await reg.pushManager.getSubscription())?.unsubscribe();
    },
  };
}

function notifications(platform: ReturnType<typeof detectPlatform>): NotificationsCapability {
  const supported = typeof Notification !== 'undefined';
  return {
    permission: (): PermissionState => (supported ? (Notification.permission as PermissionState) : 'unsupported'),
    async requestPermission() {
      if (!supported) return 'unsupported';
      return (await Notification.requestPermission()) as PermissionState;
    },
    async shownTags() {
      const reg = await swReady();
      if (!reg) return [];
      return (await reg.getNotifications()).map((n) => n.tag);
    },
    async close(tags) {
      const reg = await swReady();
      if (!reg) return;
      for (const n of await reg.getNotifications()) if (tags.includes(n.tag)) n.close();
    },
    setBadge(n) {
      const nav = navigator as Navigator & { setAppBadge?(n?: number): Promise<void>; clearAppBadge?(): Promise<void> };
      if (n > 0) void nav.setAppBadge?.(n).catch(() => {});
      else void nav.clearAppBadge?.().catch(() => {});
    },
    onOpen(cb) {
      if (!('serviceWorker' in navigator)) return () => {};
      const f = (e: MessageEvent) => {
        const d = e.data as { type?: string; url?: string } | null;
        if (d?.type === 'open' && typeof d.url === 'string') cb(d.url);
      };
      navigator.serviceWorker.addEventListener('message', f);
      return () => navigator.serviceWorker.removeEventListener('message', f);
    },
    // iOS/iPadOS only grants Web Push to home-screen apps (16.4+).
    needsInstallForPush: () => platform.ios && !standalone(),
  };
}

// ---- install prompt (captured at module load: the event fires early) ----

interface InstallPromptEvent extends Event {
  prompt(): Promise<void>;
}
let offer: InstallPromptEvent | null = null;
const installListeners = new Set<() => void>();
window.addEventListener('beforeinstallprompt', (e) => {
  e.preventDefault();
  offer = e as InstallPromptEvent;
  installListeners.forEach((f) => f());
});
window.addEventListener('appinstalled', () => {
  offer = null;
  installListeners.forEach((f) => f());
});

function install(platform: ReturnType<typeof detectPlatform>): InstallCapability {
  return {
    canPrompt: () => offer !== null,
    async prompt() {
      const o = offer;
      offer = null;
      installListeners.forEach((f) => f());
      await o?.prompt().catch(() => {});
    },
    subscribe(cb) {
      installListeners.add(cb);
      return () => installListeners.delete(cb);
    },
    standalone: standalone(),
    iosShareSheet: platform.ios,
  };
}

// ---- speech ----

interface Recognition {
  continuous: boolean;
  interimResults: boolean;
  lang: string;
  onresult: ((e: { results: ArrayLike<ArrayLike<{ transcript: string }> & { isFinal: boolean }> }) => void) | null;
  onerror: ((e: unknown) => void) | null;
  onend: (() => void) | null;
  start(): void;
  stop(): void;
  abort(): void;
}

function speech(): SpeechCapability {
  const w = window as unknown as { SpeechRecognition?: new () => Recognition; webkitSpeechRecognition?: new () => Recognition };
  const Rec = w.SpeechRecognition ?? w.webkitSpeechRecognition;
  const canRecord = typeof MediaRecorder !== 'undefined' && !!navigator.mediaDevices?.getUserMedia;
  return {
    recognizer: !!Rec,
    recognize: Rec
      ? (onPartial) => {
          const r = new Rec();
          r.continuous = true;
          r.interimResults = true;
          r.lang = navigator.language || 'en-US';
          let text = '';
          let done: ((s: string) => void) | null = null;
          r.onresult = (e) => {
            let all = '';
            for (let i = 0; i < e.results.length; i++) all += e.results[i]![0]!.transcript;
            text = all;
            onPartial(text);
          };
          r.onend = () => done?.(text);
          r.onerror = () => done?.(text);
          r.start();
          return {
            stop: () =>
              new Promise<string>((resolve) => {
                done = resolve;
                r.stop();
                setTimeout(() => resolve(text), 1500);
              }),
            cancel: () => r.abort(),
          };
        }
      : undefined,
    recorder: canRecord,
    record: canRecord
      ? async () => {
          const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
          const mime = ['audio/webm;codecs=opus', 'audio/mp4', 'audio/ogg'].find((m) => MediaRecorder.isTypeSupported?.(m)) ?? '';
          const rec = new MediaRecorder(stream, mime ? { mimeType: mime } : undefined);
          const chunks: Blob[] = [];
          rec.ondataavailable = (e) => e.data.size && chunks.push(e.data);
          rec.start();
          const end = () => stream.getTracks().forEach((t) => t.stop());
          return {
            stop: () =>
              new Promise((resolve, reject) => {
                rec.onstop = async () => {
                  end();
                  const blob = new Blob(chunks, { type: rec.mimeType || mime || 'audio/webm' });
                  resolve({ mime: blob.type, data: new Uint8Array(await blob.arrayBuffer()) });
                };
                rec.onerror = (e) => (end(), reject(e));
                rec.stop();
              }),
            cancel: () => {
              rec.onstop = null;
              if (rec.state !== 'inactive') rec.stop();
              end();
            },
          };
        }
      : undefined,
  };
}

export function createPwaPlatform(): UiPlatform {
  persist();
  const p = detectPlatform(navigator.userAgent, navigator.platform, navigator.maxTouchPoints);
  return {
    keystore,
    hostStore,
    kv,
    connect: connectWebSocket,
    clock: systemClock,
    random: (n) => crypto.getRandomValues(new Uint8Array(n)),
    lifecycle,
    platformName: p.name,
    defaultDeviceName: p.device,
    push: pushSupport(),
    notifications: notifications(p),
    install: install(p),
    speech: speech(),
    haptics: (kind) => {
      const pattern = { tap: 8, success: [10, 30, 10], warning: [20, 40, 20], error: [30, 50, 30] }[kind];
      try {
        navigator.vibrate?.(pattern);
      } catch {
        /* unsupported */
      }
    },
    clipboard: {
      writeText: (s) => navigator.clipboard.writeText(s),
      readText: () => navigator.clipboard.readText(),
    },
    share:
      typeof navigator.share === 'function'
        ? (data) => navigator.share(data)
        : undefined,
    openExternal: (url) => {
      if (/^https?:\/\//i.test(url)) window.open(url, '_blank', 'noopener,noreferrer');
    },
    mirrorCache: {
      get: async (k) => (await idbGet<{ text: string; at: number }>('mirrors', k)) ?? null,
      set: (k, v) => idbSet('mirrors', k, v),
    },
    // Saved dashboards share the `mirrors` store (own key prefix): no schema change.
    dashboardCache: {
      get: async (host) => (await idbGet<{ at: number; dashboard: Dashboard }>('mirrors', `dashboard:${host}`)) ?? null,
      set: (host, v) => idbSet('mirrors', `dashboard:${host}`, v),
      remove: (host) => idbDelete('mirrors', `dashboard:${host}`),
    },
    build: { version: __APP_VERSION__, hash: __BUILD_HASH__, origin: location.origin },
    client: { client: 'vibeke-pwa', version: __APP_VERSION__ },
  };
}
