// Pairing over the channel (spec 16 §4.2–§4.3), device side.
//
//   hello {mode:"pair", pid} → Noise IKpsk2 (psk from the QR) → request pair.claim
//   pair.claim {name, platform, vapid_public?} → {status:"pending", fingerprint}
//                                              | {status:"done", device_id, host_name, scope} (bearer / --no-confirm)
//   later notification pair.done {device_id, host_name, scope} | pair.rejected {reason?}
//
// The fingerprint shown by the app is computed locally from the device key; the host's value is
// checked against it so a confused host cannot confirm a different key under our name.

import { Channel, ChannelError } from './channel';
import { helloPair } from './hello';
import { fingerprint, x25519Public } from './keys';
import { linkExpired, linkHostKey, linkPsk, type PairingLink } from './link';
import type { Scope } from './model';
import type { Platform } from './platform';
import { RpcClient, RpcError } from './rpc';
import type { HostRecord, HostStore } from './hosts';

export class PairingError extends Error {
  constructor(
    /** `refused`: the host answered `pair.claim` with an error (`refusal` names it). */
    readonly code: 'expired' | 'rejected' | 'fingerprint_mismatch' | 'protocol' | 'timeout' | 'channel' | 'refused',
    message: string,
    override readonly cause?: unknown,
  ) {
    super(message);
    this.name = 'PairingError';
  }

  /** The host's error kind for `refused` (`used`, `github_not_allowed`, …). */
  get refusal(): string | undefined {
    return this.code === 'refused' && this.cause instanceof RpcError ? this.cause.kind : undefined;
  }

  /** GitHub logins the link is for, from a `github_not_allowed` refusal (spec 20 §2.3). */
  get allowed(): string[] {
    const d = this.cause instanceof RpcError ? (this.cause.data?.details as { allowed?: unknown } | undefined) : undefined;
    return Array.isArray(d?.allowed) ? d.allowed.filter((x): x is string => typeof x === 'string').slice(0, 50) : [];
  }
}

/**
 * Host refusals after which the invitation can never work for this device key (spec 20 §2.4).
 * Only these delete the invitation's key; after anything else (network, timeout, host offline) the
 * key is kept, so a retry after a lost response is the same device.
 */
export const DEFINITIVE_REFUSALS: ReadonlySet<string> = new Set([
  'used',
  'expired',
  'revoked',
  'forbidden',
  'github_not_allowed',
  'attest_invalid',
  'upgrade_required',
]);

/** Should the invitation key be deleted after this pairing failure? */
export function isDefinitiveRefusal(e: unknown): boolean {
  if (!(e instanceof PairingError)) return false;
  if (e.code === 'expired' || e.code === 'rejected' || e.code === 'fingerprint_mismatch') return true;
  return e.code === 'refused' && DEFINITIVE_REFUSALS.has(e.refusal ?? '');
}

export interface PairOptions {
  link: PairingLink;
  platform: Platform;
  devicePrivate: Uint8Array;
  /** Keystore name of `devicePrivate` when it is an invitation's own key (kept on the record). */
  keyName?: string;
  /** Device display name, e.g. "Alice's iPhone". */
  deviceName: string;
  /** Device VAPID public key (base64url), if push is set up already. */
  vapidPublic?: string;
  /** Called once the host holds the claim and is waiting for confirmation. */
  onPending?(fingerprint: string): void;
  /** Overall wait for host confirmation. Default 3 min (host side times out at 2 min). */
  timeoutMs?: number;
  /** Persist the result here. */
  store?: HostStore;
  /** Tests: fixed ephemeral key. */
  ephemeral?: Uint8Array;
  /** GitHub attestation for an `access: github` invitation (spec 20 §2.3). */
  attest?: string;
}

interface Done {
  device_id: string;
  host_name?: string;
  scope: Scope;
  ticket?: string;
  ticket_exp?: number;
}

const trimSlashes = (s: string): string => {
  let end = s.length;
  while (end > 0 && s.charCodeAt(end - 1) === 47 /* '/' */) end--;
  return s.slice(0, end);
};

export const relayConnectUrl = (relay: string, hostId: string, ticket?: string): string =>
  `${trimSlashes(relay)}/v1/connect?host=${encodeURIComponent(hostId)}${ticket ? `&ticket=${encodeURIComponent(ticket)}` : ''}`;

export async function pair(o: PairOptions): Promise<HostRecord> {
  const { link, platform } = o;
  const clock = platform.clock;
  if (linkExpired(link, clock.now())) throw new PairingError('expired', 'pairing link expired');
  // A handoff invitation is accepted on one of the user's hosts (`peer.redeem`); the gateway
  // refuses an app's claim.
  if (link.share?.kind === 'handoff') throw new PairingError('protocol', 'open this invitation on one of your hosts');
  const ownFingerprint = fingerprint(x25519Public(o.devicePrivate));

  let channel: Channel;
  try {
    channel = await Channel.connect({
      socket: platform.connect(relayConnectUrl(link.relay, link.host, link.tk)),
      hello: helloPair(link.pid),
      hostKey: linkHostKey(link),
      devicePrivate: o.devicePrivate,
      psk: linkPsk(link),
      clock,
      random: (n) => platform.random(n),
      ephemeral: o.ephemeral,
    });
  } catch (e) {
    throw new PairingError('channel', (e as Error).message, e);
  }
  const rpc = new RpcClient(channel, { clock, random: (n) => platform.random(n), keepalive: false });

  try {
    const done = await new Promise<Done>((resolve, reject) => {
      const timer = clock.setTimeout(
        () => reject(new PairingError('timeout', 'host did not confirm in time')),
        o.timeoutMs ?? 180_000,
      );
      let off: (() => void)[] = [];
      let settled = false;
      const settle = (f: () => void) => {
        if (settled) return;
        settled = true;
        clock.clearTimeout(timer);
        off.forEach((x) => x());
        f();
      };
      let pendingSeen = false;
      const pending = (fp: unknown) => {
        if (typeof fp !== 'string') return;
        if (fp !== ownFingerprint) {
          settle(() => reject(new PairingError('fingerprint_mismatch', `host shows ${fp}, device is ${ownFingerprint}`)));
          return;
        }
        if (!pendingSeen) o.onPending?.(fp);
        pendingSeen = true;
      };
      const finish = (d: unknown) => {
        const r = d as Partial<Done> | null;
        if (!r || typeof r.device_id !== 'string' || !isScope(r.scope)) {
          settle(() => reject(new PairingError('protocol', 'malformed pair.done')));
        } else settle(() => resolve(r as Done));
      };
      off = [
        rpc.on('pair.pending', (p) => pending((p as { fingerprint?: unknown })?.fingerprint)),
        rpc.on('pair.done', finish),
        rpc.on('pair.rejected', (p) =>
          settle(() => reject(new PairingError('rejected', `host rejected pairing${reasonOf(p)}`))),
        ),
        channel.onClose((err) =>
          settle(() => reject(new PairingError('channel', err?.message ?? 'channel closed', err))),
        ),
      ];
      const params: Record<string, unknown> = { name: o.deviceName, platform: platform.platformName };
      if (o.vapidPublic) params.vapid_public = o.vapidPublic;
      if (o.attest) params.attest = o.attest;
      rpc.request<Record<string, unknown>>('pair.claim', params, { timeoutMs: 30_000 }).then(
        (res) => {
          if (res?.status === 'pending') pending(res.fingerprint);
          else if (res?.status === 'done') finish(res);
          else settle(() => reject(new PairingError('protocol', 'unexpected pair.claim result')));
        },
        (e) =>
          settle(() =>
            reject(new PairingError(e instanceof ChannelError ? 'channel' : e instanceof RpcError ? 'refused' : 'protocol', (e as Error).message, e)),
          ),
      );
    });

    const record: HostRecord = {
      host_id: link.host,
      relay: link.relay,
      hk: link.hk,
      device_id: done.device_id,
      name: done.host_name ?? link.name,
      scope: done.scope,
      paired_at: clock.now(),
    };
    if (link.share) {
      // Review links (spec 20 §3) are kept apart from host records by the caller (`reviewRecord`).
      record.kind = 'share';
      record.until = link.share.until;
      if (link.share.label) record.label = link.share.label;
      if (link.share.limit) record.limit = link.share.limit;
    }
    if (typeof done.ticket === 'string' && done.ticket && typeof done.ticket_exp === 'number' && Number.isFinite(done.ticket_exp)) {
      record.ticket = done.ticket;
      record.ticket_exp = done.ticket_exp;
    }
    if (o.keyName) record.key = o.keyName;
    if (typeof (done as { github?: unknown }).github === 'object') {
      const g = (done as { github?: { login?: unknown } }).github;
      if (typeof g?.login === 'string') record.github = g.login;
    }
    if (link.share?.kind !== 'review') await o.store?.put(record);
    return record;
  } finally {
    channel.close();
  }
}

const isScope = (s: unknown): s is Scope => s === 'full' || s === 'approve' || s === 'view';
const reasonOf = (p: unknown): string => {
  const r = (p as { reason?: unknown } | null)?.reason;
  return typeof r === 'string' ? `: ${r}` : '';
};
