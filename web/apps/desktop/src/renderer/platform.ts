// The desktop UiPlatform: host connections and keys live in the main process (HostEngine over
// the bridge); this window only renders. Sockets/keystore on the Platform are inert stubs:
// nothing in the renderer may open a connection or touch key material.

import { linkToUrl, systemClock, type HostRecord, type KeyStore, type Lifecycle, type Socket } from '@vibeke/core';
import type { HostEngine, KV, UiCommand, UiPlatform } from '@vibeke/ui';
import { EVENT, INVOKE, type BootInfo, type Bridge, type EngineHello } from '../shared/contract';
import { RemoteManager, call } from './remote';
import { extensions } from './extensions';

declare const __APP_VERSION__: string;
declare const __BUILD_HASH__: string;

const noKeys: KeyStore = {
  get: () => Promise.reject(new Error('keys live in the main process')),
  set: () => Promise.reject(new Error('keys live in the main process')),
  delete: () => Promise.reject(new Error('keys live in the main process')),
};

/** A socket that closes immediately: renderers never open connections. */
function noSocket(): Socket {
  const s: Socket = {
    state: 'closed',
    send() {
      throw new Error('renderer sockets are disabled');
    },
    close() {},
    onopen: null,
    onmessage: null,
    onclose: null,
  };
  queueMicrotask(() => s.onclose?.(1006, 'renderer sockets are disabled'));
  return s;
}

/** Main's window-shown signal (a hidden window can still report `visible` in some cases). */
let windowShown = true;

function lifecycle(bridge: Bridge): Lifecycle {
  bridge.on(EVENT.visibility, (v) => {
    if (typeof v === 'boolean') windowShown = v;
  });
  return {
    onVisible(cb) {
      const f = () => document.visibilityState === 'visible' && cb();
      document.addEventListener('visibilitychange', f);
      const off = bridge.on(EVENT.visibility, (v) => v === true && cb());
      return () => {
        document.removeEventListener('visibilitychange', f);
        off();
      };
    },
    onHidden(cb) {
      const f = () => document.visibilityState === 'hidden' && cb();
      document.addEventListener('visibilitychange', f);
      const off = bridge.on(EVENT.visibility, (v) => v === false && cb());
      return () => {
        document.removeEventListener('visibilitychange', f);
        off();
      };
    },
    isVisible: () => document.visibilityState === 'visible' && windowShown,
  };
}

const kv: KV = {
  get(k) {
    try {
      return localStorage.getItem(k);
    } catch {
      return null;
    }
  },
  set(k, v) {
    try {
      localStorage.setItem(k, v);
    } catch {
      /* full */
    }
  },
  remove(k) {
    try {
      localStorage.removeItem(k);
    } catch {
      /* ignore */
    }
  },
  // Every window shares the app:// origin: prefs changed in one apply to all.
  watch(k, cb) {
    const f = (e: StorageEvent) => e.key === k && cb(e.newValue);
    window.addEventListener('storage', f);
    return () => window.removeEventListener('storage', f);
  },
};

function engine(bridge: Bridge): HostEngine {
  let started: Promise<{ manager: RemoteManager; fingerprint: string }> | null = null;
  return {
    start() {
      started ??= (async () => {
        // Subscribe before asking for the snapshot: an update in between is buffered, not lost.
        const manager = new RemoteManager(bridge);
        try {
          const h = await call<EngineHello>(bridge, INVOKE.engineStart);
          manager.attach(h.hosts, h.version);
          return { manager, fingerprint: h.fingerprint };
        } catch (e) {
          manager.stop();
          throw e;
        }
      })();
      started.catch(() => (started = null));
      return started;
    },
    async pair(link, deviceName, onPending) {
      const token = crypto.randomUUID();
      const off = bridge.on(EVENT.pairPending, (p) => {
        const x = p as { token?: string; fingerprint?: string };
        if (x.token === token && typeof x.fingerprint === 'string') onPending(x.fingerprint);
      });
      try {
        // Re-serialize: the main process parses and validates the link itself.
        return await call<HostRecord>(bridge, INVOKE.pair, token, linkToUrl(link, 'vibeke://open'), deviceName);
      } finally {
        off();
      }
    },
  };
}

export function createDesktopPlatform(bridge: Bridge, boot: BootInfo): UiPlatform {
  const win = (op: Record<string, unknown>) => void bridge.invoke(INVOKE.window, op).catch(() => {});
  return {
    keystore: noKeys,
    hostStore: { list: async () => [], put: async () => {}, remove: async () => {} },
    kv,
    connect: noSocket,
    clock: systemClock,
    random: (n) => crypto.getRandomValues(new Uint8Array(n)),
    lifecycle: lifecycle(bridge),
    platformName: boot.platformName,
    defaultDeviceName: boot.deviceName,
    engine: engine(bridge),
    localTransport: true,
    localAlerts: true,
    // Pairing on the desktop is by link or local socket; the camera stays off (permission denied).
    qrScan: false,
    mac: boot.platform === 'darwin',
    clipboard: {
      writeText: (s) => call(bridge, INVOKE.clipboardWrite, s),
      readText: () => call<string>(bridge, INVOKE.clipboardRead),
    },
    openExternal: (url) => {
      if (/^https?:\/\//i.test(url)) void bridge.invoke(INVOKE.openExternal, url).catch(() => {});
    },
    pickDirectory: (defaultPath) => call<string | null>(bridge, INVOKE.pickDirectory, defaultPath),
    windows: {
      popOutPane: (host, pane) => win({ op: 'pop-out', host, pane }),
      openMain: (hash) => win({ op: 'open-main', hash }),
      close: () => win({ op: 'close' }),
    },
    onCommand: (cb) => bridge.on(EVENT.command, (c) => cb(c as UiCommand)),
    extensions: extensions(bridge, boot),
    speech: {
      recognizer: false,
      recorder: typeof MediaRecorder !== 'undefined' && !!navigator.mediaDevices?.getUserMedia,
      record: async () => {
        const stream = await navigator.mediaDevices.getUserMedia({ audio: true });
        const mime = ['audio/webm;codecs=opus', 'audio/webm', 'audio/ogg'].find((m) => MediaRecorder.isTypeSupported?.(m)) ?? '';
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
      },
    },
    build: { version: __APP_VERSION__, hash: __BUILD_HASH__, origin: `${location.origin} (bundled with the app)` },
    client: { client: 'vibeke-desktop', version: __APP_VERSION__ },
  };
}
