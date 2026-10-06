import { describe, expect, test } from 'bun:test';
import * as b64 from '../src/b64';
import { fingerprint, x25519Public } from '../src/keys';
import type { PairingLink } from '../src/link';
import { pair, PairingError } from '../src/pairing';
import type { HostRecord } from '../src/hosts';
import { serveGateway, testPlatform } from './helpers';

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s * 17 + i * 5) & 0xff);
const HOST = seeded(1);
const DEV = seeded(2);
const PSK = seeded(3);

function link(p: Partial<PairingLink> = {}): PairingLink {
  return {
    v: 1,
    relay: 'wss://relay.example',
    host: 'hostid',
    hk: b64.encode(x25519Public(HOST)),
    pid: 'pid-1',
    psk: b64.encode(PSK),
    exp: Math.floor(1_800_000_000_000 / 1000) + 600,
    name: 'devbox',
    ...p,
  };
}

describe('pair()', () => {
  test('claim → pending → done', async () => {
    let claim: any = null;
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle(method, params, ctx) {
          if (method !== 'pair.claim') return undefined;
          claim = params;
          setTimeout(() => ctx.notify('pair.done', { device_id: 'd1', host_name: 'devbox.local', scope: 'approve' }), 5);
          return { status: 'pending', fingerprint: fingerprint(ctx.deviceKey) };
        },
      }),
    );
    const stored: HostRecord[] = [];
    let shown = '';
    const rec = await pair({
      link: link(),
      platform,
      devicePrivate: DEV,
      deviceName: "the maintainer's phone",
      vapidPublic: 'BPk',
      onPending: (fp) => (shown = fp),
      store: { list: async () => stored, put: async (r) => void stored.push(r), remove: async () => {} },
    });
    expect(platform.urls).toEqual(['wss://relay.example/v1/connect?host=hostid']);
    expect(claim).toEqual({ name: "the maintainer's phone", platform: 'test', vapid_public: 'BPk' });
    expect(shown).toBe(fingerprint(x25519Public(DEV)));
    expect(rec).toMatchObject({ host_id: 'hostid', relay: 'wss://relay.example', device_id: 'd1', name: 'devbox.local', scope: 'approve' });
    expect(stored).toEqual([rec]);
  });

  test('bearer mode: claim result is already done', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle: () => ({ status: 'done', device_id: 'd2', scope: 'full' }),
      }),
    );
    const rec = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' });
    expect(rec).toMatchObject({ device_id: 'd2', name: 'devbox', scope: 'full' });
  });

  test('share invitation: pending then pair.done right away; record keeps kind/until/label/limit', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle(_m, _p, ctx) {
          queueMicrotask(() => ctx.notify('pair.done', { device_id: 'd3', host_name: 'devbox', scope: 'view' }));
          return { status: 'pending', fingerprint: fingerprint(ctx.deviceKey) };
        },
      }),
    );
    const share = { kind: 'share' as const, scope: 'view', until: 1_800_007_200, label: 'samplehub', limit: { workspace: 'w1' } };
    const rec = await pair({ link: link({ share }), platform, devicePrivate: DEV, deviceName: 'p' });
    expect(rec).toMatchObject({ device_id: 'd3', scope: 'view', kind: 'share', until: 1_800_007_200, label: 'samplehub', limit: { workspace: 'w1' } });
  });

  test('rejected', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle(_m, _p, ctx) {
          setTimeout(() => ctx.notify('pair.rejected', { reason: 'declined' }), 5);
          return { status: 'pending', fingerprint: fingerprint(ctx.deviceKey) };
        },
      }),
    );
    const e = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e).toBeInstanceOf(PairingError);
    expect(e.code).toBe('rejected');
    expect(e.message).toContain('declined');
  });

  test('fingerprint mismatch aborts', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, { hostPrivate: HOST, psks: { 'pid-1': PSK }, handle: () => ({ status: 'pending', fingerprint: 'aaaa-bbbb' }) }),
    );
    const e = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e.code).toBe('fingerprint_mismatch');
  });

  test('wrong psk / expired link', async () => {
    const platform = testPlatform((sock) => serveGateway(sock, { hostPrivate: HOST, psks: { 'pid-1': seeded(4) } }));
    const e = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e.code).toBe('channel');
    const e2 = await pair({ link: link({ exp: 1 }), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e2.code).toBe('expired');
  });
});
