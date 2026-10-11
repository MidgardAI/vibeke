import { describe, expect, test } from 'bun:test';
import { ed25519 } from '@noble/curves/ed25519.js';
import * as b64 from '../src/b64';
import { checkCode, hostId, keyBindingMessage, x25519Public } from '../src/keys';
import type { PairingLink } from '../src/link';
import { pair, PairingError } from '../src/pairing';
import type { HostRecord } from '../src/hosts';
import { serveGateway, testPlatform } from './helpers';

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s * 17 + i * 5) & 0xff);
const HOST = seeded(1);
const DEV = seeded(2);
const PSK = seeded(3);
/** The host's relay (Ed25519) seed: its public key names the host and signs its Noise key. */
const RELAY = seeded(4);
const RELAY_PUB = ed25519.getPublicKey(RELAY);
const HOST_ID = hostId(RELAY_PUB);

/** The handshake payload of a real gateway: the Noise key bound to the host id. */
function hostInfo(o: { hk?: Uint8Array; relay?: Uint8Array } = {}) {
  const relay = o.relay ?? RELAY;
  return {
    v: 1,
    host_name: 'devbox',
    host_id: hostId(ed25519.getPublicKey(relay)),
    relay_key: b64.encode(ed25519.getPublicKey(relay)),
    key_sig: b64.encode(ed25519.sign(keyBindingMessage(o.hk ?? x25519Public(HOST)), relay)),
  };
}

/** What a gateway answers `pair.claim` with: the check code of this connection. */
const pendingFor = (ctx: { session: { handshakeHash: Uint8Array } }) => ({ status: 'pending', check: checkCode(ctx.session.handshakeHash) });

function link(p: Partial<PairingLink> = {}): PairingLink {
  return {
    v: 1,
    relay: 'wss://relay.example',
    host: HOST_ID,
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
        hostInfo: hostInfo(),
        psks: { 'pid-1': PSK },
        handle(method, params, ctx) {
          if (method !== 'pair.claim') return undefined;
          claim = params;
          setTimeout(() => ctx.notify('pair.done', { device_id: 'd1', host_name: 'devbox.local', scope: 'approve', ticket: 'dev.ticket', ticket_exp: 1_900_000_000 }), 5);
          return pendingFor(ctx);
        },
      }),
    );
    const stored: HostRecord[] = [];
    let shown = '';
    const rec = await pair({
      link: link({ tk: 'e30.sig-x_y' }),
      platform,
      devicePrivate: DEV,
      deviceName: "Alice's phone",
      vapidPublic: 'BPk',
      onPending: (fp) => (shown = fp),
      store: { list: async () => stored, put: async (r) => void stored.push(r), remove: async () => {} },
    });
    expect(platform.urls).toEqual([`wss://relay.example/v1/connect?host=${HOST_ID}`]);
    expect(platform.tickets).toEqual(['e30.sig-x_y']);
    expect(rec).toMatchObject({ ticket: 'dev.ticket', ticket_exp: 1_900_000_000 });
    expect(claim).toEqual({ name: "Alice's phone", platform: 'test', vapid_public: 'BPk' });
    expect(shown).toMatch(/^[A-Z2-7]{4}-[A-Z2-7]{4}-[A-Z2-7]{4}$/);
    expect(rec).toMatchObject({ host_id: HOST_ID, relay: 'wss://relay.example', device_id: 'd1', name: 'devbox.local', scope: 'approve' });
    expect(stored).toEqual([rec]);
  });

  test('bearer mode: claim result is already done', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        hostInfo: hostInfo(),
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
        hostInfo: hostInfo(),
        psks: { 'pid-1': PSK },
        handle(_m, _p, ctx) {
          queueMicrotask(() => ctx.notify('pair.done', { device_id: 'd3', host_name: 'devbox', scope: 'view' }));
          return pendingFor(ctx);
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
        hostInfo: hostInfo(),
        psks: { 'pid-1': PSK },
        handle(_m, _p, ctx) {
          setTimeout(() => ctx.notify('pair.rejected', { reason: 'declined' }), 5);
          return pendingFor(ctx);
        },
      }),
    );
    const e = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e).toBeInstanceOf(PairingError);
    expect(e.code).toBe('rejected');
    expect(e.message).toContain('declined');
  });

  test('check code mismatch aborts', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, { hostPrivate: HOST, hostInfo: hostInfo(), psks: { 'pid-1': PSK }, handle: () => ({ status: 'pending', check: 'AAAA-BBBB-CCCC' }) }),
    );
    const e = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e.code).toBe('fingerprint_mismatch');
  });

  test('a host that cannot prove the link host id is refused before claiming', async () => {
    let claimed = false;
    const handle = () => {
      claimed = true;
      return { status: 'done', device_id: 'd9', scope: 'full' };
    };
    // Another host's relay key, a binding for another Noise key, and no binding at all.
    for (const info of [hostInfo({ relay: seeded(9) }), hostInfo({ hk: x25519Public(seeded(8)) }), { v: 1, host_name: 'devbox' }]) {
      const platform = testPlatform((sock) => serveGateway(sock, { hostPrivate: HOST, hostInfo: info, psks: { 'pid-1': PSK }, handle }));
      const e = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
      expect(e.code).toBe('host_mismatch');
    }
    expect(claimed).toBe(false);
  });

  test('wrong psk / expired link', async () => {
    const platform = testPlatform((sock) => serveGateway(sock, { hostPrivate: HOST, psks: { 'pid-1': seeded(4) } }));
    const e = await pair({ link: link(), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e.code).toBe('channel');
    const e2 = await pair({ link: link({ exp: 1 }), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x: any) => x);
    expect(e2.code).toBe('expired');
  });
});
