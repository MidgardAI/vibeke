// Spec 20 links: review fields on pairing links, continue links, the attestation nonce and the
// pending sign-in across the redirect.

import { describe, expect, test } from 'bun:test';
import { sha256 } from '@noble/hashes/sha2.js';
import * as b64 from '../src/b64';
import {
  attestClaims,
  attestNonce,
  attestStartUrl,
  parseAttestQuery,
  saveAttestHandoff,
  savePendingAttest,
  takeAttestHandoff,
  takePendingAttest,
  type SmallStorage,
} from '../src/attest';
import { isReviewLink, linkJson, linkRoute, linkToUrl, needsAttestation, parseContinueLink, parseLink, shareTeamNames, type PairingLink } from '../src/link';

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
const enc = (o: unknown) => b64.encode(new TextEncoder().encode(JSON.stringify(o)));

describe('review links', () => {
  const share = {
    kind: 'review' as const,
    scope: 'view',
    until: 1_800_086_400,
    access: 'github' as const,
    uses: 20,
    notes: 'direct' as const,
    preview: { id: 'pv_1', label: 'v4' },
    sid: 'abcdefghijklmnop',
  };

  test('parse, route and round trip', () => {
    const l = { ...base, share, tk: 'a.b' };
    const url = linkToUrl(l, 'https://app.example/');
    expect(url.startsWith('https://app.example/#/review?d=')).toBe(true);
    expect(linkRoute(url)).toBe('review');
    const p = parseLink(url);
    expect(isReviewLink(p)).toBe(true);
    expect(needsAttestation(p)).toBe(true);
    expect(p.share).toMatchObject({ uses: 20, notes: 'direct', sid: 'abcdefghijklmnop', preview: { id: 'pv_1' } });
    expect(linkJson(p)).toBe(linkJson(l));
  });

  test('a GitHub share link stays a pair link', () => {
    const l = { ...base, share: { kind: 'share' as const, scope: 'view', until: 5, access: 'github' as const, uses: 3 } };
    const url = linkToUrl(l, 'https://app.example');
    expect(linkRoute(url)).toBe('pair');
    expect(needsAttestation(parseLink(url))).toBe(true);
    expect(isReviewLink(parseLink(url))).toBe(false);
  });

  test('malformed share fields are refused', () => {
    const bad = [
      { ...share, access: 'anyone' },
      { ...share, uses: 0 },
      { ...share, uses: 2.5 },
      { ...share, notes: 'later' },
      { ...share, sid: 'short' },
      { ...share, sid: 'abcdefghijklmno1' },
      { ...share, preview: { id: 5 } },
      { ...share, preview: undefined },
      { ...share, account: 'javascript:alert(1)' },
      { ...share, teams: 'Acme' },
      { ...share, teams: [{ name: 'Acme' }] },
      { ...share, teams: [{ id: '' }] },
      { ...share, teams: [{ id: 't1', name: 5 }] },
      { ...share, teams: Array.from({ length: 51 }, (_, i) => ({ id: `t${i}` })) },
    ];
    for (const s of bad) expect(() => parseLink(enc({ ...base, share: s }))).toThrow();
  });

  test('team links name their teams (spec 20 §2.5)', () => {
    const teams = [{ id: '0b5c3f7e-1d2a-4c8b-9e6f-3a1b2c3d4e5f', name: 'Acme' }, { id: '11111111-2222-4333-8444-555555555555' }];
    const l = { ...base, share: { kind: 'share' as const, scope: 'view', until: 5, access: 'github' as const, uses: 20, teams } };
    const p = parseLink(linkToUrl(l, 'https://app.example'));
    expect(p.share?.teams).toEqual(teams);
    expect(shareTeamNames(p.share)).toEqual(['Acme', '11111111-2222-4333-8444-555555555555']);
    expect(linkJson(p)).toBe(linkJson(l));
    expect(shareTeamNames(undefined)).toEqual([]);
  });

  test('older links without the new fields still parse', () => {
    const p = parseLink(enc({ ...base, share: { kind: 'share', scope: 'view', until: 5 } }));
    expect(needsAttestation(p)).toBe(false);
  });
});

describe('continue links', () => {
  const k = b64.encode(new Uint8Array(32).fill(7));
  const link = {
    v: 1,
    server: 'https://cloud.example',
    blob: 'blob_01',
    k,
    sha256: 'a'.repeat(64),
    size: 1234,
    exp: 1_900_000_000,
    brief: { repo: 'app', branch: 'feat/login', harness: 'claude', title: 'Login', last: 'x'.repeat(400), from: 'alice' },
  };

  test('parse a full URL and the bare value', () => {
    const url = `https://app.example/#/continue?d=${enc(link)}`;
    expect(linkRoute(url)).toBe('continue');
    const p = parseContinueLink(url);
    expect(p).toMatchObject({ server: 'https://cloud.example', blob: 'blob_01', size: 1234, brief: { repo: 'app', from: 'alice' } });
    expect(p.brief.last).toHaveLength(280);
    expect(parseContinueLink(enc(link)).blob).toBe('blob_01');
  });

  test('missing brief fields read as null', () => {
    const p = parseContinueLink(enc({ ...link, brief: { repo: 'app' } }));
    expect(p.brief).toEqual({ repo: 'app', branch: null, harness: null, title: null, last: null, from: null });
  });

  test('malformed links are refused', () => {
    for (const bad of [
      { ...link, v: 2 },
      { ...link, k: b64.encode(new Uint8Array(16)) },
      { ...link, sha256: 'xyz' },
      { ...link, size: -1 },
      { ...link, server: 'ftp://x' },
      { ...link, blob: '../etc' },
      { ...link, brief: { repo: 5 } },
    ]) {
      expect(() => parseContinueLink(enc(bad))).toThrow();
    }
    expect(() => parseContinueLink('not base64 !!')).toThrow();
  });
});

describe('attestation', () => {
  test('nonce = b64url(sha256("vibeke-attest/1\\0" ‖ pid ‖ "\\0" ‖ pub))', () => {
    const pub = Uint8Array.from({ length: 32 }, (_, i) => i);
    const msg = new Uint8Array([...new TextEncoder().encode('vibeke-attest/1\0pid-1\0'), ...pub]);
    expect(attestNonce('pid-1', pub)).toBe(b64.encode(sha256(msg)));
    expect(attestNonce('pid-1', pub)).not.toBe(attestNonce('pid-2', pub));
    expect(() => attestNonce('pid-1', new Uint8Array(31))).toThrow();
  });

  test('start URL', () => {
    const u = new URL(attestStartUrl('https://cloud.example/', { aud: 'host1', nonce: 'n-1', returnTo: 'https://app.example/' }));
    expect(u.origin + u.pathname).toBe('https://cloud.example/v1/attest/start');
    expect(u.searchParams.get('aud')).toBe('host1');
    expect(u.searchParams.get('nonce')).toBe('n-1');
    expect(u.searchParams.get('return')).toBe('https://app.example/');
  });

  test('attest route query', () => {
    const payload = enc({ v: 1, kid: 'k', gh_id: 42, login: 'alice', aud: 'h', nonce: 'n', iat: 1, exp: 301 });
    const token = `v1.${payload}.${'s'.repeat(86)}`;
    expect(parseAttestQuery(`t=${token}`)).toEqual({ ok: true, token });
    expect(parseAttestQuery('?error=github_denied')).toEqual({ ok: false, error: 'github_denied' });
    expect(parseAttestQuery('error=<script>')).toEqual({ ok: false, error: 'unknown' });
    expect(parseAttestQuery('t=v2.x.y')).toEqual({ ok: false, error: 'invalid' });
    expect(parseAttestQuery('')).toBeNull();
    expect(attestClaims(token)).toMatchObject({ login: 'alice', gh_id: 42 });
  });

  test('pending sign-in and result survive the redirect once', () => {
    const m = new Map<string, string>();
    const s: SmallStorage = { getItem: (k) => m.get(k) ?? null, setItem: (k, v) => void m.set(k, v), removeItem: (k) => void m.delete(k) };
    savePendingAttest(s, { route: 'review', d: 'D', pid: 'p1', nonce: 'n', at: 1000 });
    expect(takePendingAttest(s, 2000)).toMatchObject({ route: 'review', d: 'D', pid: 'p1' });
    expect(takePendingAttest(s, 2000)).toBeNull();
    savePendingAttest(s, { route: 'pair', d: 'D', pid: 'p1', nonce: 'n', at: 1000 });
    expect(takePendingAttest(s, 1000 + 11 * 60_000)).toBeNull();

    saveAttestHandoff(s, { pid: 'p1', result: { ok: false, error: 'access_denied' }, at: 1000 });
    expect(takeAttestHandoff(s, 'other', 1000)).toBeNull();
    expect(takeAttestHandoff(s, 'p1', 1000)).toEqual({ ok: false, error: 'access_denied' });
    expect(takeAttestHandoff(s, 'p1', 1000)).toBeNull();
  });
});
