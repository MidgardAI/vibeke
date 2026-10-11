// Plain words for access levels, revoke cascades and relay ticket expiry (settings, sharing).

import type { HostRecord } from '@vibeke/core';

/** Warn this long before a relay ticket expires. */
export const TICKET_WARN_MS = 7 * 86_400_000;

/**
 * Days left on a host's relay ticket when it expires within `TICKET_WARN_MS`, else null. The app
 * renews the ticket each time it connects, so a warning means it has not reached the host lately.
 */
export function ticketDaysLeft(r: Pick<HostRecord, 'ticket' | 'ticket_exp'>, nowMs: number): number | null {
  if (!r.ticket || typeof r.ticket_exp !== 'number' || !Number.isFinite(r.ticket_exp)) return null;
  const left = r.ticket_exp * 1000 - nowMs;
  if (left <= 0 || left > TICKET_WARN_MS) return null;
  return Math.max(1, Math.ceil(left / 86_400_000));
}

/** What else a revoke removes (`devices.list` `also`, or `devices.revoke` result `also`). */
export interface AlsoCounts {
  shares?: number;
  hosts?: number;
  invitations?: number;
  peers?: number;
}

export function parseAlso(v: unknown): AlsoCounts | null {
  if (!v || typeof v !== 'object') return null;
  const o = v as Record<string, unknown>;
  const n = (k: string) => (typeof o[k] === 'number' && Number.isFinite(o[k]) && (o[k] as number) > 0 ? (o[k] as number) : 0);
  return { shares: n('shares'), hosts: n('hosts'), invitations: n('invitations'), peers: n('peers') };
}

/** "This also removes 2 shares and 1 invitation." or null when nothing else goes. */
export function alsoText(
  also: AlsoCounts | null,
  w: { share(n: number): string; host(n: number): string; invitation(n: number): string; removes(list: string): string },
): string | null {
  if (!also) return null;
  const hosts = (also.hosts ?? 0) + (also.peers ?? 0);
  const parts = [
    also.shares ? w.share(also.shares) : null,
    hosts ? w.host(hosts) : null,
    also.invitations ? w.invitation(also.invitations) : null,
  ].filter((x): x is string => !!x);
  if (!parts.length) return null;
  const list = parts.length === 1 ? parts[0]! : `${parts.slice(0, -1).join(', ')} and ${parts.at(-1)}`;
  return w.removes(list);
}
