import { describe, expect, test } from 'bun:test';
import * as b64 from '../src/b64';
import { helloBytes, helloDevice, helloPair, helloText, parseHello } from '../src/hello';
import { base32, fingerprint, hostId, loadOrCreateDeviceKey, x25519Public } from '../src/keys';
import { linkExpired, linkJson, linkToUrl, parseLink, type PairingLink } from '../src/link';
import type { KeyStore } from '../src/platform';
import { MemKeyStore } from './helpers';

describe('b64', () => {
  test('roundtrip and url alphabet', () => {
    for (let n = 0; n < 40; n++) {
      const b = Uint8Array.from({ length: n }, (_, i) => (i * 37 + 250) & 0xff);
      const s = b64.encode(b);
      expect(s).not.toMatch(/[+/=]/);
      expect(b64.decode(s)).toEqual(b);
      expect(s).toBe(Buffer.from(b).toString('base64url'));
    }
  });
  test('tolerates padding, rejects junk', () => {
    expect(b64.decode('Zm8=')).toEqual(new TextEncoder().encode('fo'));
    expect(() => b64.decode('Zm+v')).toThrow();
    expect(b64.decode('Zm8')).toEqual(new TextEncoder().encode('fo'));
    expect(() => b64.decode('Zm9')).toThrow(/trailing/); // non-canonical
    expect(() => b64.decode('Zm9=x')).toThrow();
    expect(() => b64.decode('Zm9v8')).toThrow(/length/);
    expect(() => b64.decode('Zh')).toThrow(/trailing/); // non-canonical
    expect(() => b64.decodeExact('AAAA', 32)).toThrow(/32/);
  });
});

describe('keys', () => {
  test('base32 RFC 4648 vectors (lowercase, unpadded)', () => {
    const v: [string, string][] = [
      ['', ''],
      ['f', 'my'],
      ['fo', 'mzxq'],
      ['foo', 'mzxw6'],
      ['foob', 'mzxw6yq'],
      ['fooba', 'mzxw6ytb'],
      ['foobar', 'mzxw6ytboi'],
    ];
    for (const [i, o] of v) expect(base32(new TextEncoder().encode(i))).toBe(o);
  });
  test('host id and fingerprint shapes', () => {
    const k = new Uint8Array(32).fill(3);
    expect(hostId(k)).toMatch(/^[a-z2-7]{26}$/);
    expect(fingerprint(k)).toMatch(/^[a-z2-7]{4}-[a-z2-7]{4}$/);
    // blake3("") = af1349b9f5..., base32 of the first 5 bytes = "v4jutopv" (checked with Python)
    expect(fingerprint(new Uint8Array(0))).toBe('v4ju-topv');
  });
  test('device key is created once', async () => {
    const ks = new MemKeyStore();
    const a = await loadOrCreateDeviceKey(ks);
    const b = await loadOrCreateDeviceKey(ks);
    expect(a).toEqual(b);
    expect(x25519Public(a).length).toBe(32);
  });
  test('concurrent startups get the same device key (in-process serialization)', async () => {
    // A slow store: without serialization both calls would see "missing" and create two keys.
    class SlowStore extends MemKeyStore {
      override async get(n: string) {
        await new Promise((r) => setTimeout(r, 5));
        return super.get(n);
      }
    }
    const ks = new SlowStore();
    const [a, b, c] = await Promise.all([loadOrCreateDeviceKey(ks), loadOrCreateDeviceKey(ks), loadOrCreateDeviceKey(ks)]);
    expect(b).toEqual(a);
    expect(c).toEqual(a);
    expect(ks.m.get('device_static')).toEqual(a);
  });
  test('uses the store\'s atomic getOrCreate when present', async () => {
    const ks = new MemKeyStore() as MemKeyStore & { getOrCreate: KeyStore['getOrCreate'] };
    let atomic = 0;
    ks.getOrCreate = async (name, make, valid) => {
      atomic++;
      const v = ks.m.get(name);
      if (v && valid(v)) return v;
      const k = make();
      ks.m.set(name, k);
      return k;
    };
    const a = await loadOrCreateDeviceKey(ks);
    const b = await loadOrCreateDeviceKey(ks);
    expect(b).toEqual(a);
    expect(atomic).toBe(2);
  });
});

describe('hello', () => {
  test('byte-identical to serde', () => {
    expect(helloText(helloDevice())).toBe('{"v":1,"proto":"vibeke-e2e/1","mode":"device"}');
    expect(helloText(helloPair('pid-1'))).toBe('{"v":1,"proto":"vibeke-e2e/1","mode":"pair","pid":"pid-1"}');
    // Field order is canonical regardless of construction order.
    expect(helloText({ pid: 'p', mode: 'pair', proto: 'vibeke-e2e/1', v: 1 })).toBe(
      '{"v":1,"proto":"vibeke-e2e/1","mode":"pair","pid":"p"}',
    );
    expect(new TextDecoder().decode(helloBytes(helloDevice()))).toBe(helloText(helloDevice()));
  });
  test('parse validates', () => {
    expect(parseHello(helloText(helloPair('x')))).toEqual(helloPair('x'));
    expect(() => parseHello('{"v":2,"proto":"vibeke-e2e/1","mode":"device"}')).toThrow('unsupported_version');
    expect(() => parseHello('{"v":1,"proto":"vibeke-e2e/1","mode":"pair"}')).toThrow(/pid/);
    expect(() => parseHello('{"v":1,"proto":"vibeke-e2e/1","mode":"device","pid":"x"}')).toThrow(/pid/);
  });
});

describe('pairing link', () => {
  const link: PairingLink = {
    v: 1,
    relay: 'wss://r.example',
    host: 'abc',
    hk: b64.encode(new Uint8Array(32).fill(1)),
    pid: 'p1',
    psk: b64.encode(new Uint8Array(32).fill(2)),
    exp: 99,
    name: 'devbox',
  };

  test('JSON matches serde field order', () => {
    expect(linkJson(link)).toBe(
      `{"v":1,"relay":"wss://r.example","host":"abc","hk":"${link.hk}","pid":"p1","psk":"${link.psk}","exp":99,"name":"devbox"}`,
    );
  });

  test('roundtrip like link.rs', () => {
    const url = linkToUrl(link, 'https://r.example/');
    expect(url.startsWith('https://r.example/#/pair?d=')).toBe(true);
    expect(parseLink(url)).toEqual(link);
    expect(parseLink(url.split('d=')[1]!)).toEqual(link);
    expect(parseLink(`${url}&x=1`)).toEqual(link);
  });

  test('rejects bad links', () => {
    const d = (o: unknown) => b64.encode(new TextEncoder().encode(JSON.stringify(o)));
    expect(() => parseLink(d({ ...link, v: 2 }))).toThrow('unsupported_version');
    expect(() => parseLink(d({ ...link, hk: 'AAAA' }))).toThrow(/32 bytes/);
    expect(() => parseLink(d({ ...link, name: undefined }))).toThrow(/name/);
    expect(() => parseLink('#/pair?d=!!!')).toThrow(/link/);
  });

  test('share invitations carry their share payload (round trip, validated)', () => {
    const share = { kind: 'share' as const, scope: 'view', until: 1_900_000_000, label: 'samplehub', limit: { workspace: 'w1' } };
    const l = { ...link, share };
    const url = linkToUrl(l, 'https://r.example/');
    expect(parseLink(url)).toEqual(l);
    expect(linkJson(l).endsWith(`"name":"devbox","share":${JSON.stringify(share)}}`)).toBe(true);
    const d = (o: unknown) => b64.encode(new TextEncoder().encode(JSON.stringify(o)));
    expect(() => parseLink(d({ ...link, share: { kind: 'root', scope: 'full', until: 1 } }))).toThrow(/share/);
    expect(() => parseLink(d({ ...link, share: { kind: 'share', scope: 'view' } }))).toThrow(/share/);
    expect(parseLink(d({ ...link, share: null }))).toEqual(link);
  });

  test('expiry', () => {
    expect(linkExpired(link, 98_000)).toBe(false);
    expect(linkExpired(link, 99_000)).toBe(true);
  });
});
