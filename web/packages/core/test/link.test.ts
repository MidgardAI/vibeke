import { describe, expect, test } from 'bun:test';
import * as b64 from '../src/b64';
import { linkJson, linkToUrl, parseLink, type PairingLink } from '../src/link';

const base: PairingLink = {
  v: 1,
  relay: 'wss://relay.example',
  host: 'hostid',
  hk: b64.encode(new Uint8Array(32).fill(1)),
  pid: 'pid-1',
  psk: b64.encode(new Uint8Array(32).fill(2)),
  exp: 1_800_000_600,
  name: 'devbox',
};

describe('pairing link', () => {
  test('tk is emitted after share, only when present', () => {
    expect(linkJson(base)).not.toContain('"tk"');
    const share = { kind: 'share' as const, scope: 'view', until: 5 };
    const json = linkJson({ ...base, share, tk: 'abc.def' });
    expect(Object.keys(JSON.parse(json))).toEqual(['v', 'relay', 'host', 'hk', 'pid', 'psk', 'exp', 'name', 'share', 'tk']);
    expect(json.endsWith('"tk":"abc.def"}')).toBe(true);
  });

  test('round trip is byte-identical', () => {
    for (const l of [{ ...base, tk: 'abc.def' }, { ...base, share: { kind: 'share' as const, scope: 'view', until: 5 }, tk: 'x_y-z.q' }, base]) {
      const parsed = parseLink(linkToUrl(l, 'https://app.example'));
      expect(linkJson(parsed)).toBe(linkJson(l));
    }
  });

  test('rejects a malformed tk', () => {
    const d = b64.encode(new TextEncoder().encode(JSON.stringify({ ...base, tk: 'a b' })));
    expect(() => parseLink(d)).toThrow('bad tk');
  });
});
