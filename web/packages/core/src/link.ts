// The pairing link carried by the QR code (spec 16 §4.1), mirroring vk-e2e/src/link.rs.
// `<app>/#/pair?d=<base64url(json)>`; the payload lives in the fragment, never sent to the server.

import * as b64 from './b64';
import { VERSION } from './hello';

export interface PairingLink {
  v: number;
  /** Relay WebSocket base, e.g. `wss://relay.example.com`. */
  relay: string;
  /** Host id on the relay. */
  host: string;
  /** Host Noise static public key (base64url). */
  hk: string;
  /** Pairing id (not secret). */
  pid: string;
  /** Pairing secret (base64url, 32 bytes). */
  psk: string;
  /** Expiry, unix seconds. */
  exp: number;
  /** Host display name. */
  name: string;
  /** Present on share/handoff invitations (spec 16 §15). */
  share?: ShareInvite;
}

export type HostKind = 'device' | 'share';

/** What a share/handoff invitation link says about itself (spec 16 §15.1). */
export interface ShareInvite {
  kind: 'share' | 'handoff';
  /** Scope the resulting device gets (`view`/`approve` for shares, `full` for handoff). */
  scope: string;
  /** Unix seconds the device stops working (approximate: counted from link creation). */
  until: number;
  label?: string | null;
  limit?: { workspace?: string; pane?: string } | null;
}

/** Validate the optional `share` object; anything malformed is rejected rather than ignored. */
function parseShare(v: unknown): ShareInvite | undefined {
  if (v === undefined || v === null) return undefined;
  const o = v as Record<string, unknown>;
  if (typeof o !== 'object' || (o.kind !== 'share' && o.kind !== 'handoff')) throw new Error('link: bad share');
  if (typeof o.scope !== 'string' || typeof o.until !== 'number' || !Number.isFinite(o.until)) throw new Error('link: bad share');
  if (o.label !== undefined && o.label !== null && typeof o.label !== 'string') throw new Error('link: bad share');
  return v as ShareInvite;
}

const STRING_FIELDS = ['relay', 'host', 'hk', 'pid', 'psk', 'name'] as const;

/** JSON in serde field order (so links round-trip byte-identically with Rust). */
export function linkJson(l: PairingLink): string {
  const { v, relay, host, hk, pid, psk, exp, name, share } = l;
  return JSON.stringify(share ? { v, relay, host, hk, pid, psk, exp, name, share } : { v, relay, host, hk, pid, psk, exp, name });
}

export function linkToUrl(l: PairingLink, appBase: string): string {
  const d = b64.encode(new TextEncoder().encode(linkJson(l)));
  return `${appBase.replace(/\/+$/, '')}/#/pair?d=${d}`;
}

/** Accept a full URL or the bare `d` value. */
export function parseLink(s: string): PairingLink {
  let d = s;
  const i = s.indexOf('d=');
  if (i >= 0 && (s.includes('#') || s.includes('?'))) d = s.slice(i + 2);
  d = d.split('&')[0]!;
  let raw: unknown;
  try {
    raw = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(b64.decode(d)));
  } catch (e) {
    throw new Error(`link: ${(e as Error).message}`);
  }
  const l = raw as Record<string, unknown>;
  if (typeof l !== 'object' || l === null) throw new Error('link: not an object');
  if (typeof l.v !== 'number' || typeof l.exp !== 'number' || !Number.isInteger(l.exp) || l.exp < 0) {
    throw new Error('link: bad v/exp');
  }
  for (const f of STRING_FIELDS) if (typeof l[f] !== 'string') throw new Error(`link: missing ${f}`);
  if (l.v !== VERSION) throw new Error('unsupported_version');
  const link = l as unknown as PairingLink;
  linkHostKey(link);
  linkPsk(link);
  const share = parseShare(l.share);
  const out: PairingLink = { v: link.v, relay: link.relay, host: link.host, hk: link.hk, pid: link.pid, psk: link.psk, exp: link.exp, name: link.name };
  if (share) out.share = share;
  return out;
}

export const linkHostKey = (l: PairingLink): Uint8Array => b64.decodeExact(l.hk, 32);
export const linkPsk = (l: PairingLink): Uint8Array => b64.decodeExact(l.psk, 32);

/** True when the link has expired at `nowMs`. */
export const linkExpired = (l: PairingLink, nowMs: number): boolean => nowMs / 1000 >= l.exp;
