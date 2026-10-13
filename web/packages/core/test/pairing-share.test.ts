// pair.claim for GitHub-gated and review invitations (spec 20 §2.3, §2.4).

import { describe, expect, test } from 'bun:test';
import * as b64 from '../src/b64';
import { fingerprint, x25519Public } from '../src/keys';
import type { PairingLink } from '../src/link';
import { isDefinitiveRefusal, pair, PairingError } from '../src/pairing';
import type { HostRecord } from '../src/hosts';
import { serveGateway, testPlatform } from './helpers';

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s * 13 + i * 7) & 0xff);
const HOST = seeded(1);
const DEV = seeded(2);
const PSK = seeded(3);

const link = (share: PairingLink['share']): PairingLink => ({
  v: 1,
  relay: 'wss://relay.example',
  host: 'hostid',
  hk: b64.encode(x25519Public(HOST)),
  pid: 'pid-1',
  psk: b64.encode(PSK),
  exp: Math.floor(1_800_000_000_000 / 1000) + 600,
  name: 'devbox',
  share,
});

const review = { kind: 'review' as const, scope: 'view', until: 1_800_086_400, access: 'github' as const, uses: 20, preview: { id: 'pv_1' }, sid: 'abcdefghijklmnop' };

describe('pair() with share links', () => {
  test('sends the attestation and keeps the GitHub login; review records are not stored as hosts', async () => {
    let claim: any = null;
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle(method, params) {
          if (method !== 'pair.claim') return undefined;
          claim = params;
          return { status: 'done', device_id: 'd1', scope: 'view', github: { id: 42, login: 'alice' } };
        },
      }),
    );
    const stored: HostRecord[] = [];
    const rec = await pair({
      link: link(review),
      platform,
      devicePrivate: DEV,
      deviceName: 'p',
      attest: 'v1.payload.sig',
      store: { list: async () => stored, put: async (r) => void stored.push(r), remove: async () => {} },
    });
    expect(claim).toMatchObject({ attest: 'v1.payload.sig' });
    expect(rec).toMatchObject({ device_id: 'd1', kind: 'share', github: 'alice' });
    expect(stored).toEqual([]);
  });

  test('host refusals become `refused` with the kind and the allowed logins', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle() {
          throw { code: -32003, message: 'not allowed', data: { kind: 'github_not_allowed', details: { allowed: ['alice', 'bob', 7] } } };
        },
      }),
    );
    const e = await pair({ link: link(review), platform, devicePrivate: DEV, deviceName: 'p', attest: 'v1.a.b' }).catch((x) => x);
    expect(e).toBeInstanceOf(PairingError);
    expect(e.code).toBe('refused');
    expect(e.refusal).toBe('github_not_allowed');
    expect(e.allowed).toEqual(['alice', 'bob']);
    expect(isDefinitiveRefusal(e)).toBe(true);
  });

  test('only definitive refusals drop the invitation key', () => {
    expect(isDefinitiveRefusal(new PairingError('timeout', 'slow'))).toBe(false);
    expect(isDefinitiveRefusal(new PairingError('channel', 'offline'))).toBe(false);
    expect(isDefinitiveRefusal(new PairingError('protocol', 'odd'))).toBe(false);
    expect(isDefinitiveRefusal(new PairingError('expired', 'late'))).toBe(true);
    expect(isDefinitiveRefusal(new PairingError('rejected', 'no'))).toBe(true);
    expect(isDefinitiveRefusal(new Error('x'))).toBe(false);
  });

  test('a transient host error is not definitive', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle() {
          throw { code: -32000, message: 'busy', data: { kind: 'unavailable' } };
        },
      }),
    );
    const e = await pair({ link: link(review), platform, devicePrivate: DEV, deviceName: 'p' }).catch((x) => x);
    expect(e.code).toBe('refused');
    expect(isDefinitiveRefusal(e)).toBe(false);
  });

  test('pending then done for a plain share still works', async () => {
    const platform = testPlatform((sock) =>
      serveGateway(sock, {
        hostPrivate: HOST,
        psks: { 'pid-1': PSK },
        handle(_m, _p, ctx) {
          queueMicrotask(() => ctx.notify('pair.done', { device_id: 'd3', scope: 'view' }));
          return { status: 'pending', fingerprint: fingerprint(ctx.deviceKey) };
        },
      }),
    );
    const rec = await pair({ link: link({ kind: 'share', scope: 'view', until: 1_800_007_200 }), platform, devicePrivate: DEV, deviceName: 'p' });
    expect(rec).toMatchObject({ device_id: 'd3', kind: 'share' });
  });
});
