import { describe, expect, test } from 'bun:test';
import { p256 } from '@noble/curves/nist.js';
import * as b64 from '../src/b64';
import { HostManager, type HostRecord, type HostStore } from '../src/hosts';
import { x25519Public } from '../src/keys';
import type { PushSubscriptionInfo, PushSupport } from '../src/platform';
import { PushSync, VAPID_KEY_NAME, generateVapid, loadOrCreateVapid } from '../src/push';
import { MemKeyStore, flush, serveGateway, testPlatform } from './helpers';
import { randomBytes } from '@noble/hashes/utils.js';

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s * 11 + i * 3) & 0xff);
const HOST = seeded(1);
const DEV = seeded(2);

const rec = (id: string): HostRecord => ({
  host_id: id,
  relay: 'wss://relay.example',
  hk: b64.encode(x25519Public(HOST)),
  device_id: 'd',
  name: id,
  scope: 'full',
});

function fakePush(): PushSupport & { subs: number; current: PushSubscriptionInfo | null; keyUsed: string | null } {
  let n = 0;
  const p = {
    subs: 0,
    current: null as PushSubscriptionInfo | null,
    keyUsed: null as string | null,
    async getSubscription() {
      return p.current;
    },
    async subscribe(key: string) {
      p.subs++;
      p.keyUsed = key;
      p.current = { endpoint: `https://web.push.apple.com/ep${++n}`, keys: { p256dh: 'pk', auth: 'au' } };
      return p.current;
    },
    async unsubscribe() {
      p.current = null;
    },
  };
  return p;
}

function setup(hosts: string[], shares: string[] = [], legacy?: { cacheAlerts: boolean; failAfterSave?: boolean }) {
  const calls: [string, string, any][] = [];
  const devicePrefs = { privacy: 'minimal', notify_input: true, notify_done: true, notify_cache_cold: legacy?.cacheAlerts };
  const platform = testPlatform((sock, url) => {
    const host = new URL(url.replace('wss:', 'https:')).searchParams.get('host')!;
    serveGateway(sock, {
      hostPrivate: HOST,
      handle(method, params) {
        calls.push([host, method, params]);
        if (method === 'hello') return { host_name: host, device_id: 'd', scope: 'full', server_version: '1', features: legacy ? ['cache_cold'] : [] };
        if (method === 'dashboard.get') return { at: 1, workspaces: [], tabs: [], panes: [], runs: [], interactions: [], tasks: [] };
        if (method === 'events.subscribe') return { at: 1 };
        if (method === 'prefs.get') return { device: devicePrefs, host: { dnd_until: 0 } };
        if (method === 'prefs.set') {
          Object.assign(devicePrefs, params.device);
          if (legacy?.failAfterSave) {
            legacy.failAfterSave = false;
            throw { code: -32000, message: 'Reply failed after saving' };
          }
        }
        return {};
      },
    });
  });
  const records = [...hosts.map(rec), ...shares.map((id): HostRecord => ({ ...rec(id), kind: 'share' }))];
  const store: HostStore = { list: async () => records, put: async () => {}, remove: async () => {} };
  const manager = new HostManager({ platform, store, devicePrivate: DEV, client: { client: 't', version: '0' } });
  const push = fakePush();
  const keystore = new MemKeyStore();
  const sync = new PushSync({ manager, push, keystore, random: randomBytes });
  return { calls, manager, push, keystore, sync, devicePrefs };
}

describe('VAPID keys', () => {
  test('concurrent calls return the same key', async () => {
    class SlowStore extends MemKeyStore {
      override async get(n: string) {
        await new Promise((r) => setTimeout(r, 5));
        return super.get(n);
      }
    }
    const ks = new SlowStore();
    const [a, b] = await Promise.all([loadOrCreateVapid(ks, randomBytes), loadOrCreateVapid(ks, randomBytes)]);
    expect(b.privateKey).toEqual(a.privateKey);
    expect(ks.m.get(VAPID_KEY_NAME)).toEqual(a.privateKey);
  });

  test('generated once and persisted; public key is the uncompressed point', async () => {
    const ks = new MemKeyStore();
    const a = await loadOrCreateVapid(ks, randomBytes);
    const b = await loadOrCreateVapid(ks, randomBytes);
    expect(b.privateKey).toEqual(a.privateKey);
    expect(a.publicKey.length).toBe(65);
    expect(a.publicKey[0]).toBe(4);
    expect(Array.from(p256.getPublicKey(a.privateKey, false))).toEqual(Array.from(a.publicKey));
    expect(ks.m.get(VAPID_KEY_NAME)?.length).toBe(32);
    expect(generateVapid(randomBytes).privateKey).not.toEqual(a.privateKey);
  });
});

describe('PushSync', () => {
  test('enable subscribes with the device key and sends to every online host', async () => {
    const { calls, manager, push, sync } = setup(['h1', 'h2']);
    await manager.start();
    await flush(30);
    await sync.start();
    expect(sync.state).toBe('off');
    await sync.enable();
    expect(sync.state).toBe('on');
    const subs = calls.filter((c) => c[1] === 'push.subscribe');
    expect(subs.map((c) => c[0]).sort()).toEqual(['h1', 'h2']);
    const keys = await loadOrCreateVapid(sync['o'].keystore, randomBytes);
    expect(push.keyUsed).toBe(b64.encode(keys.publicKey));
    expect(subs[0]![2].vapid_private).toBe(b64.encode(keys.privateKey));
    expect(subs[0]![2].subscription.endpoint).toBe('https://web.push.apple.com/ep1');
    expect(typeof subs[0]![2].op_id).toBe('string');
    expect(calls.some((c) => c[1].startsWith('prefs.'))).toBe(false);
    manager.stop();
  });

  test('existing subscriptions disable retired cache alerts on older hosts once', async () => {
    const s = setup(['own'], ['shared'], { cacheAlerts: true });
    await s.manager.start();
    await flush(30);
    const keys = await loadOrCreateVapid(s.keystore, randomBytes);
    s.push.current = { endpoint: 'https://web.push.apple.com/existing', keys: { p256dh: 'pk', auth: 'au' } };
    await s.keystore.set('push_sent', new TextEncoder().encode(JSON.stringify({ own: `${s.push.current.endpoint}|${b64.encode(keys.publicKey)}` })));

    try {
      await s.sync.start();
      const changes = s.calls.filter((c) => c[1] === 'prefs.set');
      expect(changes).toHaveLength(1);
      expect(changes[0]![0]).toBe('own');
      expect(changes[0]![2].device).toEqual({ notify_cache_cold: false });
      expect(typeof changes[0]![2].op_id).toBe('string');
      expect(s.devicePrefs).toEqual({ privacy: 'minimal', notify_input: true, notify_done: true, notify_cache_cold: false });
      expect(s.calls.some((c) => c[0] === 'shared' && c[1].startsWith('prefs.'))).toBe(false);

      s.sync.stop();
      const again = new PushSync({ manager: s.manager, push: s.push, keystore: s.keystore, random: randomBytes });
      await again.start();
      expect(s.calls.filter((c) => c[1] === 'prefs.get')).toHaveLength(1);
      expect(s.calls.filter((c) => c[1] === 'push.subscribe')).toHaveLength(1);
      again.stop();
    } finally {
      s.sync.stop();
      s.manager.stop();
    }
  });

  test('an uncertain preference update is read back before another sync', async () => {
    const s = setup(['h1'], [], { cacheAlerts: true, failAfterSave: true });
    await s.manager.start();
    await flush(30);
    try {
      await s.sync.start();
      await s.sync.enable();
      expect(s.calls.filter((c) => c[1] === 'prefs.set')).toHaveLength(1);
      expect(s.calls.filter((c) => c[1] === 'push.subscribe')).toHaveLength(0);
      await s.sync.syncAll();
      expect(s.calls.filter((c) => c[1] === 'prefs.get')).toHaveLength(2);
      expect(s.calls.filter((c) => c[1] === 'prefs.set')).toHaveLength(1);
      expect(s.calls.filter((c) => c[1] === 'push.subscribe')).toHaveLength(1);
    } finally {
      s.sync.stop();
      s.manager.stop();
    }
  });

  test('supports_clear is sent only when the browser accepts silent pushes', async () => {
    for (const supportsClear of [true, false]) {
      const s = setup(['h1']);
      s.push.supportsClear = supportsClear;
      await s.manager.start();
      await flush(30);
      await s.sync.start();
      await s.sync.enable();
      const sub = s.calls.find((c) => c[1] === 'push.subscribe')!;
      expect(sub[2].supports_clear).toBe(supportsClear ? true : undefined);
      s.manager.stop();
    }
  });

  test('app start re-sends only when the endpoint changed', async () => {
    const s = setup(['h1']);
    await s.manager.start();
    await flush(30);
    await s.sync.start();
    await s.sync.enable();
    expect(s.calls.filter((c) => c[1] === 'push.subscribe').length).toBe(1);

    // Restart with the same subscription: nothing re-sent.
    const again = new PushSync({ manager: s.manager, push: s.push, keystore: s.keystore, random: randomBytes });
    await again.start();
    await flush(10);
    expect(s.calls.filter((c) => c[1] === 'push.subscribe').length).toBe(1);

    // Browser rotated the endpoint: re-sent.
    s.push.current = { endpoint: 'https://web.push.apple.com/new', keys: { p256dh: 'x', auth: 'y' } };
    const third = new PushSync({ manager: s.manager, push: s.push, keystore: s.keystore, random: randomBytes });
    await third.start();
    await flush(10);
    const subs = s.calls.filter((c) => c[1] === 'push.subscribe');
    expect(subs.length).toBe(2);
    expect(subs[1]![2].subscription.endpoint).toBe('https://web.push.apple.com/new');
    s.manager.stop();
  });

  test('rotate makes a new key, resubscribes and re-sends', async () => {
    const s = setup(['h1']);
    await s.manager.start();
    await flush(30);
    await s.sync.start();
    await s.sync.enable();
    const first = s.push.keyUsed;
    await s.sync.rotate();
    expect(s.push.keyUsed).not.toBe(first);
    expect(s.push.subs).toBe(2);
    expect(s.calls.filter((c) => c[1] === 'push.subscribe').length).toBe(2);
    await s.sync.disable();
    expect(s.calls.some((c) => c[1] === 'push.unsubscribe')).toBe(true);
    expect(s.sync.state).toBe('off');
    s.manager.stop();
  });

  test('share hosts never receive the VAPID key or push calls', async () => {
    const s = setup(['own'], ['shared']);
    await s.manager.start();
    await flush(30);
    expect(s.manager.connections().every((c) => c.getSnapshot().status === 'online')).toBe(true);
    await s.sync.start();
    await s.sync.enable();
    await s.sync.syncAll(true);
    await s.sync.rotate();
    await s.sync.disable();
    const push = s.calls.filter((c) => c[1].startsWith('push.'));
    expect(push.length).toBeGreaterThan(0);
    expect(push.every((c) => c[0] === 'own')).toBe(true);
    expect(s.calls.some((c) => c[0] === 'shared' && JSON.stringify(c[2] ?? {}).includes('vapid_private'))).toBe(false);
    s.manager.stop();
  });
});
