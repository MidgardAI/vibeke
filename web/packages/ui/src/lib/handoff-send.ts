// Sending a handoff (spec 16 §15.2): which hosts a pane's work can go to, whether the app must
// pair the two hosts first, and how an outgoing job (`handoff.job`) reads. Pure: the send sheet
// (screens/handoff.tsx) and the background job tracker (app/handoff-stores.ts) build on it.

import { hostKind, type AppEvent, type HandoffJob, type HandoffJobState, type HandoffPeer, type HostState, type IncomingHandoff, type PeerInfo, type PeerOwner } from '@vibeke/core';

/** A peer as the sheet needs it; `host` (the peer's host id) comes from `peer.list`. */
export interface KnownPeer {
  id: string;
  name: string;
  owner: PeerOwner;
  /** Unix seconds; null: never. */
  expires_at: number | null;
  expired?: boolean;
  host?: string;
}

/**
 * The source's peers: `handoff.peers` (what the sender can use) annotated with the host ids
 * `peer.list` knows. Either list may be missing (older gateway, failed call).
 */
export function mergePeers(handoffPeers: readonly HandoffPeer[] | null, peerList: readonly PeerInfo[] | null): KnownPeer[] {
  const listed = peerList ?? [];
  if (!handoffPeers) return listed.map((p) => ({ id: p.id, name: p.name, owner: p.owner, expires_at: p.expires_at ?? null, expired: p.expired, host: p.host }));
  return handoffPeers.map((p) => {
    const info = listed.find((x) => x.id === p.id) ?? listed.find((x) => x.name === p.name);
    return {
      id: p.id,
      name: p.name,
      owner: p.owner,
      expires_at: p.expires_at ?? info?.expires_at ?? null,
      ...(info?.expired !== undefined ? { expired: info.expired } : {}),
      ...(info?.host ? { host: info.host } : {}),
    };
  });
}

/** One of the user's own hosts with full access (it can invite, redeem and send). */
export const isOwnFullHost = (h: HostState): boolean => hostKind(h.record) === 'device' && (h.info?.scope ?? h.record.scope) === 'full';

const hostName = (h: HostState): string => h.info?.host_name ?? h.record.name;

export interface SendDest {
  key: string;
  /** The source's peer id; null: not paired yet (the app pairs the hosts first). */
  peer: string | null;
  /** The app's own connection to that host, when it has one. */
  hostId: string | null;
  name: string;
  owner: PeerOwner;
  /** Unix seconds; null: never. */
  expiresAt: number | null;
  expired: boolean;
  /** Unpaired hosts: whether the app reaches it now (peers are reached by the source itself). */
  online: boolean;
}

/**
 * Where `sourceHostId`'s work can go: the source's peers (own hosts first, then teammates), then
 * the user's other full-access hosts that are not peers yet (choosing one pairs them).
 */
export function sendDestinations(sourceHostId: string, peers: readonly KnownPeer[], hosts: readonly HostState[], nowS: number): SendDest[] {
  const own = hosts.filter((h) => h.record.host_id !== sourceHostId && isOwnFullHost(h));
  const out: SendDest[] = [];
  const covered = new Set<string>();
  for (const p of peers) {
    if (p.host === sourceHostId) continue;
    const h = p.host ? own.find((x) => x.record.host_id === p.host) : undefined;
    if (h) covered.add(h.record.host_id);
    out.push({
      key: `peer:${p.id}`,
      peer: p.id,
      hostId: h?.record.host_id ?? null,
      name: p.name,
      owner: p.owner,
      expiresAt: p.expires_at,
      expired: p.expired ?? (p.expires_at !== null && p.expires_at <= nowS),
      online: h ? h.status === 'online' : true,
    });
  }
  const unpaired: SendDest[] = own
    .filter((h) => !covered.has(h.record.host_id))
    .map((h) => ({ key: `host:${h.record.host_id}`, peer: null, hostId: h.record.host_id, name: hostName(h), owner: 'self', expiresAt: null, expired: false, online: h.status === 'online' }));
  const rank = (d: SendDest) => (d.peer === null ? 2 : d.owner === 'self' ? 0 : 1);
  const byName = (a: SendDest, b: SendDest) => rank(a) - rank(b) || a.name.localeCompare(b.name);
  return [...out.sort(byName), ...unpaired.sort(byName)];
}

export type SendPlan =
  /** Send to this peer. */
  | { k: 'send'; peer: string }
  /** Pair first: `peer.invite` on `hostId`, `peer.redeem` on the source, then send. */
  | { k: 'pair'; hostId: string }
  | { k: 'unavailable'; reason: 'expired' | 'offline' };

export function planSend(d: SendDest): SendPlan {
  if (d.expired) return { k: 'unavailable', reason: 'expired' };
  if (d.peer) return { k: 'send', peer: d.peer };
  if (d.hostId && d.online) return { k: 'pair', hostId: d.hostId };
  return { k: 'unavailable', reason: 'offline' };
}

// ---- jobs --------------------------------------------------------------------------------------

const JOB_STATES: readonly HandoffJobState[] = ['queued', 'exporting', 'sending', 'delivered', 'failed', 'cancelled'];
const INCOMING_STATES = ['pending', 'importing', 'imported', 'failed', 'declined'] as const;

const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) && v >= 0 ? v : 0);

/** A job from a result or event payload, or null when it is not one. */
export function parseJob(v: unknown): HandoffJob | null {
  if (typeof v !== 'object' || v === null) return null;
  const o = v as Record<string, unknown>;
  if (typeof o.id !== 'string' || !o.id) return null;
  if (!JOB_STATES.includes(o.state as HandoffJobState)) return null;
  const inc = (INCOMING_STATES as readonly unknown[]).includes(o.incoming_state) ? (o.incoming_state as HandoffJob['incoming_state']) : null;
  const err = typeof o.error === 'string' || (typeof o.error === 'object' && o.error !== null) ? (o.error as HandoffJob['error']) : null;
  return {
    id: o.id,
    pane: typeof o.pane === 'string' ? o.pane : '',
    peer: typeof o.peer === 'string' ? o.peer : '',
    peer_name: typeof o.peer_name === 'string' && o.peer_name ? o.peer_name : typeof o.peer === 'string' ? o.peer : '',
    state: o.state as HandoffJobState,
    sent: num(o.sent),
    total: num(o.total),
    incoming_state: inc,
    error: err,
    ...(typeof o.created_at_ms === 'number' ? { created_at_ms: o.created_at_ms } : {}),
    ...(typeof o.updated_at_ms === 'number' ? { updated_at_ms: o.updated_at_ms } : {}),
  };
}

/** The job a `handoff.job` event carries (`data` is the job, or `{job}`). */
export function jobFromEvent(e: AppEvent): HandoffJob | null {
  if (e.type !== 'handoff.job') return null;
  const d = e.data as Record<string, unknown>;
  const raw = typeof d.job === 'object' && d.job !== null ? d.job : d;
  const id = (raw as Record<string, unknown>).id ?? e.subject.job;
  return parseJob({ ...(raw as Record<string, unknown>), id });
}

/** Replace the job with the same id unless the copy we hold is newer. */
export function upsertJob(list: readonly HandoffJob[], j: HandoffJob): HandoffJob[] {
  const i = list.findIndex((x) => x.id === j.id);
  if (i < 0) return [...list, j];
  const old = list[i]!;
  if (old.updated_at_ms !== undefined && j.updated_at_ms !== undefined && j.updated_at_ms < old.updated_at_ms) return [...list];
  const next = [...list];
  next[i] = j;
  return next;
}

/** No more progress will come from the source (delivered, failed or cancelled). */
export const jobFinal = (j: HandoffJob): boolean => j.state === 'delivered' || j.state === 'failed' || j.state === 'cancelled';

export type JobPhase = 'queued' | 'exporting' | 'sending' | 'pending' | 'importing' | 'imported' | 'delivered' | 'import_failed' | 'declined' | 'failed' | 'cancelled';

export interface JobView {
  phase: JobPhase;
  /** 0–100 while sending (null when the size is not known yet). */
  pct: number | null;
  sent: number;
  total: number;
  error: string | null;
}

/** How a job reads: its phase (delivered jobs by what the destination made of them) and progress. */
export function jobView(j: HandoffJob): JobView {
  const pct = j.total > 0 ? Math.min(100, Math.floor((j.sent / j.total) * 100)) : null;
  let phase: JobPhase;
  switch (j.state) {
    case 'delivered':
      phase =
        j.incoming_state === 'pending'
          ? 'pending'
          : j.incoming_state === 'importing'
            ? 'importing'
            : j.incoming_state === 'imported'
              ? 'imported'
              : j.incoming_state === 'failed'
                ? 'import_failed'
                : j.incoming_state === 'declined'
                  ? 'declined'
                  : 'delivered';
      break;
    default:
      phase = j.state;
  }
  return { phase, pct: j.state === 'sending' ? (pct ?? 0) : pct, sent: j.sent, total: j.total, error: jobError(j) };
}

export function jobError(j: HandoffJob): string | null {
  const e = j.error;
  if (!e) return null;
  if (typeof e === 'string') return e;
  return e.message ?? e.kind ?? null;
}

/**
 * The incoming record a delivered job became on a destination the app can see: the newest one
 * from `sourceHost` updated since the job started (and on its branch, when known).
 */
export function findIncomingForJob(list: readonly IncomingHandoff[], sourceHost: string, sinceMs: number, branch?: string | null): IncomingHandoff | null {
  // Clocks of two hosts differ a little.
  const since = sinceMs - 120_000;
  const hits = list.filter((r) => r.from.host === sourceHost && r.created_at_ms >= since && (!branch || r.manifest.branch === branch));
  hits.sort((a, b) => b.created_at_ms - a.created_at_ms);
  return hits[0] ?? null;
}
