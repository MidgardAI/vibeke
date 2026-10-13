// GitHub identity attestation, browser side (spec 20 §2.3).
//
// 1. The app makes the invitation's own static key and computes
//    nonce = b64url(sha256("vibeke-attest/1\0" ‖ pid ‖ "\0" ‖ device_static_pub)).
// 2. It saves the pending pairing in session storage and sends the user to
//    `<account>/v1/attest/start?aud=<relay host id>&nonce=<nonce>&return=<app origin>/`.
// 3. The account server comes back to `<return>#/attest?t=<attestation>` (or `?error=<code>`).
// 4. `pair.claim` carries the attestation; the gateway checks it.
//
// The attestation is opaque to the app (the host verifies it). `attestClaims` only reads the
// login for display and never trusts it.

import { sha256 } from '@noble/hashes/sha2.js';
import * as b64 from './b64';

/** The account server used when neither the link nor the shell names one. */
export const DEFAULT_ACCOUNT_SERVER = 'https://cloud.vibeke.dev';

const NONCE_PREFIX = 'vibeke-attest/1\0';

/** The nonce that binds an attestation to one invitation (`pid`) and one device key. */
export function attestNonce(pid: string, devicePublic: Uint8Array): string {
  if (devicePublic.length !== 32) throw new Error('attest: device key must be 32 bytes');
  const enc = new TextEncoder();
  const head = enc.encode(NONCE_PREFIX);
  const id = enc.encode(pid);
  const msg = new Uint8Array(head.length + id.length + 1 + 32);
  msg.set(head, 0);
  msg.set(id, head.length);
  msg[head.length + id.length] = 0;
  msg.set(devicePublic, head.length + id.length + 1);
  return b64.encode(sha256(msg));
}

const trimSlash = (s: string): string => s.replace(/\/+$/, '');

/** Where to send the browser to sign in with GitHub. `returnTo` is the app origin plus `/`. */
export function attestStartUrl(account: string, o: { aud: string; nonce: string; returnTo: string }): string {
  const q = new URLSearchParams({ aud: o.aud, nonce: o.nonce, return: o.returnTo });
  return `${trimSlash(account)}/v1/attest/start?${q.toString()}`;
}

const TOKEN = /^v1\.[A-Za-z0-9_-]{8,4096}\.[A-Za-z0-9_-]{16,256}$/;
const CODE = /^[a-z0-9_]{1,64}$/;

export type AttestResult = { ok: true; token: string } | { ok: false; error: string };

/**
 * The query of `#/attest?t=…` or `#/attest?error=…` (the part after `?`), or null when it carries
 * neither. A malformed token reads as `error: "invalid"`.
 */
export function parseAttestQuery(query: string): AttestResult | null {
  const q = new URLSearchParams(query.startsWith('?') ? query.slice(1) : query);
  const t = q.get('t');
  if (t !== null) return TOKEN.test(t) ? { ok: true, token: t } : { ok: false, error: 'invalid' };
  const e = q.get('error');
  if (e !== null) return { ok: false, error: CODE.test(e) ? e : 'unknown' };
  return null;
}

/** The unverified claims of a token, for display only (the host verifies the signature). */
export function attestClaims(token: string): { login?: string; gh_id?: number; exp?: number; nonce?: string; aud?: string } | null {
  if (!TOKEN.test(token)) return null;
  try {
    const json = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(b64.decode(token.split('.')[1]!))) as Record<string, unknown>;
    if (typeof json !== 'object' || json === null) return null;
    const out: { login?: string; gh_id?: number; exp?: number; nonce?: string; aud?: string } = {};
    if (typeof json.login === 'string') out.login = json.login;
    if (typeof json.gh_id === 'number') out.gh_id = json.gh_id;
    if (typeof json.exp === 'number') out.exp = json.exp;
    if (typeof json.nonce === 'string') out.nonce = json.nonce;
    if (typeof json.aud === 'string') out.aud = json.aud;
    return out;
  } catch {
    return null;
  }
}

// ---- pending sign-in across the redirect (session storage) --------------------------------

/** The part of `Storage` this needs (session storage in the browser). */
export interface SmallStorage {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
  removeItem(key: string): void;
}

/** A pairing waiting for its attestation: which screen to resume and its link. */
export interface PendingAttest {
  route: 'pair' | 'review';
  /** The link's `d` value. */
  d: string;
  /** The invitation's pairing id (the nonce was computed for it). */
  pid: string;
  nonce: string;
  /** Unix ms the sign-in started. */
  at: number;
}

/** The attestation result handed from `#/attest` to the screen it resumes. */
export interface AttestHandoff {
  pid: string;
  result: AttestResult;
  at: number;
}

const PENDING_KEY = 'vibeke:attest:pending';
const RESULT_KEY = 'vibeke:attest:result';
/** A sign-in that takes longer than this is abandoned (the attestation itself lives 5 minutes). */
export const ATTEST_PENDING_MS = 10 * 60_000;

const read = <T>(s: SmallStorage, key: string): T | null => {
  try {
    const v = s.getItem(key);
    return v ? (JSON.parse(v) as T) : null;
  } catch {
    return null;
  }
};

export function savePendingAttest(s: SmallStorage, p: PendingAttest): void {
  s.setItem(PENDING_KEY, JSON.stringify(p));
}

/** Read and remove the pending sign-in; null when there is none or it is too old. */
export function takePendingAttest(s: SmallStorage, nowMs: number): PendingAttest | null {
  const p = read<PendingAttest>(s, PENDING_KEY);
  try {
    s.removeItem(PENDING_KEY);
  } catch {
    // storage unavailable: nothing to remove
  }
  if (!p || (p.route !== 'pair' && p.route !== 'review') || typeof p.d !== 'string' || typeof p.pid !== 'string' || typeof p.at !== 'number') return null;
  if (nowMs - p.at > ATTEST_PENDING_MS || p.at > nowMs + 60_000) return null;
  return p;
}

export function saveAttestHandoff(s: SmallStorage, h: AttestHandoff): void {
  s.setItem(RESULT_KEY, JSON.stringify(h));
}

/** The attestation result for `pid`, removed once read; null when none (or another invitation's). */
export function takeAttestHandoff(s: SmallStorage, pid: string, nowMs: number): AttestResult | null {
  const h = read<AttestHandoff>(s, RESULT_KEY);
  if (!h || h.pid !== pid) return null;
  try {
    s.removeItem(RESULT_KEY);
  } catch {
    // ignore
  }
  if (typeof h.at !== 'number' || nowMs - h.at > ATTEST_PENDING_MS) return null;
  const r = h.result;
  if (r?.ok === true && typeof r.token === 'string' && TOKEN.test(r.token)) return { ok: true, token: r.token };
  if (r?.ok === false && typeof r.error === 'string') return { ok: false, error: r.error };
  return null;
}
