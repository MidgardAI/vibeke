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
  /** Relay ticket for the pairing connection (`pid:` subject); opaque, URL-safe. */
  tk?: string;
}

export type HostKind = 'device' | 'share';

/** Who may open a share (spec 20 §2.2): the first opener, or listed GitHub accounts. */
export type ShareAccess = 'link' | 'github';

/** What a share/handoff invitation link says about itself (spec 16 §15.1, spec 20 §2.1). */
export interface ShareInvite {
  /** `review`: a design review of one preview (spec 20 §3). */
  kind: 'share' | 'handoff' | 'review';
  /** Scope the resulting device gets (`view`/`approve` for shares, `full` for handoff). */
  scope: string;
  /** Unix seconds the device stops working (approximate: counted from link creation). */
  until: number;
  label?: string | null;
  limit?: { workspace?: string; pane?: string } | null;
  /** Absent: `link` (older hosts). `github`: the opener proves a GitHub account first (§2.3). */
  access?: ShareAccess;
  /** How many devices the link may produce (multi-use GitHub links, §2.4). */
  uses?: number;
  /** Review links: notes go straight to the agent, or wait for the owner (§3.3). */
  notes?: 'direct' | 'held';
  /** Review links: the preview under review. */
  preview?: { id: string; label?: string | null };
  /** Review links: the per-link content origin label (`<sid>.p.<domain>`, §3.6). */
  sid?: string;
  /** Optional account server for the attestation (default: the app's configured one). */
  account?: string;
  /**
   * GitHub links: the teams whose members may open it (spec 20 §2.5). For display only; the host
   * checks membership with the signed attestation.
   */
  teams?: ShareTeam[];
}

/** A team a GitHub link is for: the id counts, the name is for display. */
export interface ShareTeam {
  id: string;
  name?: string;
}

/** Most teams one link names. */
const MAX_TEAMS = 50;

/** The names of a link's teams, for "This link is for: … everyone in Acme". */
export const shareTeamNames = (s: ShareInvite | undefined): string[] =>
  (s?.teams ?? []).map((t) => t.name || t.id).filter((n) => n.length > 0);

/** 16 base32 characters: the label of a review link's content origin. */
const SID = /^[A-Za-z2-7]{16}$/;

const optString = (v: unknown): boolean => v === undefined || v === null || typeof v === 'string';

/** Validate the optional `share` object; anything malformed is rejected rather than ignored. */
function parseShare(v: unknown): ShareInvite | undefined {
  if (v === undefined || v === null) return undefined;
  const o = v as Record<string, unknown>;
  if (typeof o !== 'object' || (o.kind !== 'share' && o.kind !== 'handoff' && o.kind !== 'review')) throw new Error('link: bad share');
  if (typeof o.scope !== 'string' || typeof o.until !== 'number' || !Number.isFinite(o.until)) throw new Error('link: bad share');
  if (!optString(o.label)) throw new Error('link: bad share');
  if (o.access !== undefined && o.access !== 'link' && o.access !== 'github') throw new Error('link: bad share access');
  if (o.uses !== undefined && (typeof o.uses !== 'number' || !Number.isInteger(o.uses) || o.uses < 1 || o.uses > 1000)) throw new Error('link: bad share uses');
  if (o.notes !== undefined && o.notes !== 'direct' && o.notes !== 'held') throw new Error('link: bad share notes');
  if (o.sid !== undefined && (typeof o.sid !== 'string' || !SID.test(o.sid))) throw new Error('link: bad share sid');
  if (o.account !== undefined && (typeof o.account !== 'string' || !/^https?:\/\/[^\s#?]+$/.test(o.account))) throw new Error('link: bad share account');
  if (o.preview !== undefined && o.preview !== null) {
    const p = o.preview as Record<string, unknown>;
    if (typeof p !== 'object' || typeof p.id !== 'string' || !p.id || p.id.length > 200 || !optString(p.label)) throw new Error('link: bad share preview');
  }
  if (o.teams !== undefined) {
    if (!Array.isArray(o.teams) || o.teams.length > MAX_TEAMS) throw new Error('link: bad share teams');
    for (const t of o.teams as unknown[]) {
      const x = t as Record<string, unknown>;
      if (typeof x !== 'object' || x === null || typeof x.id !== 'string' || !x.id || x.id.length > 64) throw new Error('link: bad share teams');
      if (x.name !== undefined && (typeof x.name !== 'string' || x.name.length > 200)) throw new Error('link: bad share teams');
    }
  }
  if (o.kind === 'review' && (typeof o.sid !== 'string' || !o.preview)) throw new Error('link: review link without preview');
  return v as ShareInvite;
}

/** A design-review link (`#/review?d=`): it opens the review screen, not the host list. */
export const isReviewLink = (l: PairingLink): boolean => l.share?.kind === 'review';

/** The link needs a GitHub sign-in through the account server before `pair.claim` (§2.3). */
export const needsAttestation = (l: PairingLink): boolean => l.share?.access === 'github';

const STRING_FIELDS = ['relay', 'host', 'hk', 'pid', 'psk', 'name'] as const;

/** JSON in serde field order (so links round-trip byte-identically with Rust). */
export function linkJson(l: PairingLink): string {
  const { v, relay, host, hk, pid, psk, exp, name, share, tk } = l;
  const o: Record<string, unknown> = { v, relay, host, hk, pid, psk, exp, name };
  if (share) o.share = share;
  if (tk !== undefined) o.tk = tk;
  return JSON.stringify(o);
}

export function linkToUrl(l: PairingLink, appBase: string): string {
  const d = b64.encode(new TextEncoder().encode(linkJson(l)));
  let end = appBase.length;
  while (end > 0 && appBase[end - 1] === '/') end--;
  return `${appBase.slice(0, end)}/#/${isReviewLink(l) ? 'review' : 'pair'}?d=${d}`;
}

/** The `d` value of a full link URL (`…#/pair?d=`, `…#/review?d=`, `…#/continue?d=`), or `s` itself. */
export function linkData(s: string): string {
  let d = s.trim();
  const m = /[?&#]d=([^&#\s]*)/.exec(d);
  if (m && (d.includes('#') || d.includes('?'))) d = m[1]!;
  return d.split('&')[0]!;
}

/** Accept a full URL or the bare `d` value. */
export function parseLink(s: string): PairingLink {
  const d = linkData(s);
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
  if (l.tk !== undefined && l.tk !== null) {
    if (typeof l.tk !== 'string' || !/^[A-Za-z0-9_.-]+$/.test(l.tk)) throw new Error('link: bad tk');
    out.tk = l.tk;
  }
  return out;
}

export const linkHostKey = (l: PairingLink): Uint8Array => b64.decodeExact(l.hk, 32);
export const linkPsk = (l: PairingLink): Uint8Array => b64.decodeExact(l.psk, 32);

/** True when the link has expired at `nowMs`. */
export const linkExpired = (l: PairingLink, nowMs: number): boolean => nowMs / 1000 >= l.exp;

// ---- continue links (spec 20 §4.2) ---------------------------------------------------------

/** What the continue screen shows before anything is downloaded. */
export interface ContinueBrief {
  repo: string | null;
  branch: string | null;
  harness: string | null;
  title: string | null;
  /** The agent's last message (at most 280 characters). */
  last: string | null;
  /** The sender's GitHub login. */
  from: string | null;
}

/** `<app>/#/continue?d=<b64url json>`: an encrypted session bundle on the account server. */
export interface ContinueLink {
  v: 1;
  /** Account server URL. */
  server: string;
  /** Blob id. */
  blob: string;
  /** 32-byte key, base64url. */
  k: string;
  /** Hex SHA-256 of the plaintext bundle. */
  sha256: string;
  /** Plaintext size in bytes. */
  size: number;
  /** Unix seconds the blob expires. */
  exp: number;
  brief: ContinueBrief;
}

const BRIEF_FIELDS = ['repo', 'branch', 'harness', 'title', 'last', 'from'] as const;

/** Parse and check a continue link (full URL or bare `d`). Throws on anything malformed. */
export function parseContinueLink(s: string): ContinueLink {
  const d = linkData(s);
  if (d.length > 8000) throw new Error('continue link: too long');
  let raw: unknown;
  try {
    raw = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(b64.decode(d)));
  } catch (e) {
    throw new Error(`continue link: ${(e as Error).message}`);
  }
  const o = raw as Record<string, unknown>;
  if (typeof o !== 'object' || o === null || Array.isArray(o)) throw new Error('continue link: not an object');
  if (o.v !== 1) throw new Error('unsupported_version');
  if (typeof o.server !== 'string' || !/^https?:\/\/[^\s#?]+$/.test(o.server)) throw new Error('continue link: bad server');
  if (typeof o.blob !== 'string' || !/^[A-Za-z0-9_-]{1,128}$/.test(o.blob)) throw new Error('continue link: bad blob');
  if (typeof o.k !== 'string') throw new Error('continue link: bad key');
  b64.decodeExact(o.k, 32);
  if (typeof o.sha256 !== 'string' || !/^[0-9a-f]{64}$/.test(o.sha256)) throw new Error('continue link: bad sha256');
  for (const f of ['size', 'exp'] as const) {
    const n = o[f];
    if (typeof n !== 'number' || !Number.isInteger(n) || n < 0) throw new Error(`continue link: bad ${f}`);
  }
  const b = (o.brief ?? {}) as Record<string, unknown>;
  if (typeof b !== 'object' || b === null || Array.isArray(b)) throw new Error('continue link: bad brief');
  const brief = {} as ContinueBrief;
  for (const f of BRIEF_FIELDS) {
    const v = b[f];
    if (v !== undefined && v !== null && typeof v !== 'string') throw new Error(`continue link: bad brief.${f}`);
    brief[f] = typeof v === 'string' ? v.slice(0, f === 'last' ? 280 : 200) : null;
  }
  return { v: 1, server: o.server, blob: o.blob, k: o.k, sha256: o.sha256, size: o.size as number, exp: o.exp as number, brief };
}

/** True when the continue link's blob has expired at `nowMs`. */
export const continueExpired = (l: ContinueLink, nowMs: number): boolean => nowMs / 1000 >= l.exp;

/** The kind of app link a pasted string is, from its route (`#/pair`, `#/review`, `#/continue`). */
export function linkRoute(s: string): 'pair' | 'review' | 'continue' | null {
  const m = /#\/(pair|review|continue)\?/.exec(s) ?? /^vibeke:\/*(pair|review|continue)\?/.exec(s.trim());
  return m ? (m[1] as 'pair' | 'review' | 'continue') : null;
}
