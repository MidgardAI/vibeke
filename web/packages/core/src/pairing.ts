// Pairing over the channel (spec 16 §4.2–§4.3), device side.
//
//   hello {mode:"pair", pid} → Noise IKpsk2 (psk from the QR) → request pair.claim
//   pair.claim {name, platform, vapid_public?} → {status:"pending", check, fingerprint}
//                                              | {status:"done", device_id, host_name, scope} (bearer / --no-confirm)
//   later notification pair.done {device_id, host_name, scope} | pair.rejected {reason?}
//
// The check code the app shows comes from this handshake (`checkCode` of the Noise handshake
// hash): the operator compares it with the code the host shows, so a party in the middle (a
// copied link, a hostile relay) can't pair unnoticed. The host's code must equal ours.
//
// Before anything is claimed, the host proves that the host id in the link is its own: its relay
// key signed its Noise key (`relay_key`, `key_sig` in the handshake payload). A crafted link can't
// pair another key under a known host's id.

import { Channel, ChannelError } from './channel';
import { helloPair } from './hello';
import * as b64 from './b64';
import { checkCode, verifyKeyBinding } from './keys';
import { linkExpired, linkHostKey, linkPsk, type PairingLink } from './link';
import type { Scope } from './model';
import type { Platform } from './platform';
import { RpcClient } from './rpc';
import type { HostRecord, HostStore } from './hosts';

export class PairingError extends Error {
  constructor(
    readonly code: 'expired' | 'rejected' | 'fingerprint_mismatch' | 'host_mismatch' | 'protocol' | 'timeout' | 'channel',
    message: string,
    override readonly cause?: unknown,
  ) {
    super(message);
    this.name = 'PairingError';
  }
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
  /**
   * Called once the host holds the claim and is waiting for confirmation, with the pairing's
   * check code (`ABCD-EFGH-IJKL`) that the host shows too.
   */
  onPending?(code: string): void;
  /** Overall wait for host confirmation. Default 3 min (host side times out at 2 min). */
  timeoutMs?: number;
  /** Persist the result here. */
  store?: HostStore;
  /** Tests: fixed ephemeral key. */
  ephemeral?: Uint8Array;
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
  if (!hostBound(link, channel.host)) {
    channel.close();
    throw new PairingError('host_mismatch', `the host could not prove that it is ${link.host}; the link may be forged`);
  }
  const ownCode = checkCode(channel.handshakeHash);
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
      const pending = (code: unknown) => {
        if (typeof code !== 'string') {
          settle(() => reject(new PairingError('protocol', 'the host sent no check code; update Vibeke on the host')));
          return;
        }
        if (code !== ownCode) {
          settle(() => reject(new PairingError('fingerprint_mismatch', `host shows ${code}, this device ${ownCode}`)));
          return;
        }
        if (!pendingSeen) o.onPending?.(ownCode);
        pendingSeen = true;
      };
      const finish = (d: unknown) => {
        const r = d as Partial<Done> | null;
        if (!r || typeof r.device_id !== 'string' || !isScope(r.scope)) {
          settle(() => reject(new PairingError('protocol', 'malformed pair.done')));
        } else settle(() => resolve(r as Done));
      };
      off = [
        rpc.on('pair.pending', (p) => pending((p as { check?: unknown })?.check)),
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
      rpc.request<Record<string, unknown>>('pair.claim', params, { timeoutMs: 30_000 }).then(
        (res) => {
          if (res?.status === 'pending') pending(res.check);
          else if (res?.status === 'done') finish(res);
          else settle(() => reject(new PairingError('protocol', 'unexpected pair.claim result')));
        },
        (e) => settle(() => reject(new PairingError(e instanceof ChannelError ? 'channel' : 'protocol', (e as Error).message, e))),
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
    await o.store?.put(record);
    return record;
  } finally {
    channel.close();
  }
}

/**
 * Whether the host's handshake payload proves that the link's host id is the host we just
 * authenticated: its relay key (whose hash is the host id) signed its Noise key.
 */
export function hostBound(link: PairingLink, info: unknown): boolean {
  const i = info as { host_id?: unknown; relay_key?: unknown; key_sig?: unknown } | null;
  if (!i || typeof i.relay_key !== 'string' || typeof i.key_sig !== 'string') return false;
  if (i.host_id !== undefined && i.host_id !== link.host) return false;
  try {
    return verifyKeyBinding(link.host, b64.decode(i.relay_key), linkHostKey(link), b64.decode(i.key_sig));
  } catch {
    return false;
  }
}

const isScope = (s: unknown): s is Scope => s === 'full' || s === 'approve' || s === 'view';
const reasonOf = (p: unknown): string => {
  const r = (p as { reason?: unknown } | null)?.reason;
  return typeof r === 'string' ? `: ${r}` : '';
};
