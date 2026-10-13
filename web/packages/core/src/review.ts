// Review connections (spec 20 §3.2, §7.2). A review device does not use the normal host
// bootstrap: it never calls `dashboard.get` or `events.subscribe`. It connects, says `hello`,
// reads `review.info`, then polls `browser.annotations {preview}` every 3 s for the state of its
// notes. Review connections are stored apart from host records, keyed by `(host_id, pid)`, so a
// review never replaces an owner or watch connection, and two reviews on one host stay separate.

import * as b64 from './b64';
import { Channel, ChannelError } from './channel';
import { helloDevice } from './hello';
import { NotConnectedError, type HostConnectionApi, type HostInfo, type HostRecord, type HostState } from './hosts';
import { recordKey } from './keys';
import { MUTATING_METHODS, type AnnotationDraft, type AppApi, type AppMethod, type ReviewInfo } from './model';
import { relayConnectUrl } from './pairing';
import type { Platform, TimerHandle } from './platform';
import { RpcClient, RpcError, type RequestOptions, type RpcLink } from './rpc';

/** A paired review, as the device stores it. */
export interface ReviewRecord extends HostRecord {
  /** The invitation's pairing id (with `host_id`, the record's key). */
  pid: string;
  sid: string;
  preview: { id: string; label?: string | null };
  notes?: 'direct' | 'held';
  /** Drafts this device sent, newest last, so their state stays listed after a reload. */
  sent?: string[];
}

export const reviewKey = (hostId: string, pid: string): string => `${hostId}/${pid}`;

/** Persistence for review records (IndexedDB in the PWA), apart from `HostStore`. */
export interface ReviewStore {
  list(): Promise<ReviewRecord[]>;
  put(record: ReviewRecord): Promise<void>;
  remove(hostId: string, pid: string): Promise<void>;
}

/** The record for a freshly paired review link. */
export function reviewRecord(
  host: HostRecord,
  link: { pid: string; share?: { sid?: string; preview?: { id: string; label?: string | null }; notes?: 'direct' | 'held' } },
): ReviewRecord {
  const s = link.share;
  if (!s?.sid || !s.preview) throw new Error('not a review link');
  const r: ReviewRecord = { ...host, kind: 'share', pid: link.pid, sid: s.sid.toLowerCase(), preview: { id: s.preview.id, label: s.preview.label ?? null } };
  if (s.notes) r.notes = s.notes;
  return r;
}

export const REVIEW_POLL_MS = 3000;
/** Sent drafts remembered per review. */
const MAX_SENT = 50;

export interface ReviewState extends HostState {
  record: ReviewRecord;
  review: ReviewInfo | null;
  /** This device's current (open) draft. */
  draft: AnnotationDraft | null;
  /** Drafts sent from this device, newest first, with their latest state. */
  sent: AnnotationDraft[];
}

export interface ReviewConnectionOptions {
  platform: Platform;
  devicePrivate: Uint8Array;
  client: { client: string; version: string };
  persist?(r: ReviewRecord): Promise<void>;
  backoffMinMs?: number;
  backoffMaxMs?: number;
  pollMs?: number;
}

const TERMINAL = new Set(['delivered']);

/** One review connection: hello → review.info → notes polling. Implements the screen-facing host API. */
export class ReviewConnection implements HostConnectionApi {
  private state: ReviewState;
  private readonly listeners = new Set<() => void>();
  private rpc: RpcClient | null = null;
  private running = false;
  private generation = 0;
  private attempts = 0;
  private unauthorized = 0;
  private retryTimer: TimerHandle | null = null;
  private pollTimer: TimerHandle | null = null;
  private sentDrafts = new Map<string, AnnotationDraft>();

  constructor(
    record: ReviewRecord,
    private readonly o: ReviewConnectionOptions,
  ) {
    this.state = {
      record,
      status: 'idle',
      error: null,
      closeCode: null,
      info: null,
      dashboard: null,
      cursor: null,
      lastOnlineAt: null,
      nextRetryAt: null,
      review: null,
      draft: null,
      sent: [],
    };
  }

  get id(): string {
    return this.state.record.host_id;
  }

  getSnapshot = (): ReviewState => this.state;

  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };

  private set(patch: Partial<ReviewState>): void {
    this.state = { ...this.state, ...patch };
    for (const cb of [...this.listeners]) cb();
  }

  start(): void {
    if (this.running) return;
    this.running = true;
    void this.connect();
  }

  stop(): void {
    this.running = false;
    this.generation++;
    this.clearTimers();
    this.rpc?.close();
    this.rpc = null;
    this.set({ status: 'idle', nextRetryAt: null });
  }

  reconnectNow(): void {
    if (!this.running) return this.start();
    const s = this.state.status;
    if (s === 'online' || s === 'connecting' || s === 'revoked' || s === 'expired' || s === 'incompatible') return;
    this.attempts = 0;
    if (this.retryTimer !== null) this.o.platform.clock.clearTimeout(this.retryTimer);
    this.retryTimer = null;
    void this.connect();
  }

  /** The live connection for stream users (the tunnel), or null while not online. */
  streamLink(): RpcLink | null {
    return this.rpc && !this.rpc.closed && this.state.status === 'online' ? this.rpc : null;
  }

  request<M extends AppMethod>(method: M, params: AppApi[M]['params'] & { op_id?: string }, opts: Omit<RequestOptions, 'mutating'> = {}): Promise<AppApi[M]['result']> {
    const rpc = this.rpc;
    if (!rpc || this.state.status !== 'online') return Promise.reject(new NotConnectedError(this.id));
    return rpc.request(method, params as Record<string, unknown>, { ...opts, mutating: MUTATING_METHODS.has(method) });
  }

  /** Re-read the notes now (after a pick or a send). */
  refresh(): Promise<void> {
    const rpc = this.rpc;
    return rpc && this.state.status === 'online' ? this.poll(rpc) : Promise.resolve();
  }

  /** Remember a draft this device sent, so its state stays listed (held, queued, delivered). */
  async rememberSent(draft: AnnotationDraft): Promise<void> {
    this.sentDrafts.set(draft.id, draft);
    const ids = [...(this.state.record.sent ?? []).filter((x) => x !== draft.id), draft.id].slice(-MAX_SENT);
    const record = { ...this.state.record, sent: ids };
    this.set({ record, sent: this.sentList() });
    await this.o.persist?.(record).catch(() => {});
  }

  private sentList(): AnnotationDraft[] {
    const order = this.state.record.sent ?? [];
    return order
      .map((id) => this.sentDrafts.get(id))
      .filter((d): d is AnnotationDraft => !!d)
      .reverse();
  }

  private async connect(): Promise<void> {
    const gen = ++this.generation;
    const { platform } = this.o;
    const { clock } = platform;
    const rec = this.state.record;
    if (rec.until !== undefined && clock.now() / 1000 >= rec.until) {
      this.running = false;
      this.set({ status: 'expired', error: null });
      return;
    }
    this.set({ status: 'connecting', nextRetryAt: null });
    let devicePrivate: Uint8Array;
    try {
      devicePrivate = await recordKey(platform.keystore, rec, this.o.devicePrivate);
    } catch (e) {
      if (gen !== this.generation) return;
      this.running = false;
      this.set({ status: 'revoked', error: (e as Error).message });
      return;
    }
    if (gen !== this.generation) return;
    let channel: Channel;
    try {
      channel = await Channel.connect({
        socket: platform.connect(relayConnectUrl(rec.relay, rec.host_id, rec.ticket)),
        hello: helloDevice(),
        hostKey: b64.decodeExact(rec.hk, 32),
        devicePrivate,
        clock,
        random: (n) => platform.random(n),
      });
    } catch (e) {
      if (gen === this.generation) this.onDisconnected(e);
      return;
    }
    if (gen !== this.generation) return channel.close();
    const rpc = new RpcClient(channel, { clock, random: (n) => platform.random(n) });
    this.rpc = rpc;
    rpc.on('device.revoked', () => {
      this.running = false;
      this.set({ status: 'revoked', error: 'device revoked by host' });
      rpc.close();
    });
    channel.onClose((err) => {
      if (gen === this.generation && this.rpc === rpc) this.onDisconnected(err);
    });
    try {
      const info = await rpc.request<HostInfo & { review?: unknown }>('hello', {
        client: this.o.client.client,
        version: this.o.client.version,
        visible: platform.lifecycle.isVisible(),
      });
      if (gen !== this.generation) return;
      const review = await rpc.request<ReviewInfo>('review.info', {});
      if (gen !== this.generation) return;
      this.set({ info, review: normalizeReviewInfo(review) });
      // Renew the relay ticket (best effort; older gateways do not have it).
      void rpc.request<{ ticket?: unknown; exp?: unknown }>('relay.ticket', {}).then(
        (r) => {
          if (gen !== this.generation || typeof r?.ticket !== 'string' || typeof r.exp !== 'number') return;
          const record = { ...this.state.record, ticket: r.ticket, ticket_exp: r.exp };
          this.set({ record });
          void this.o.persist?.(record).catch(() => {});
        },
        () => {},
      );
      this.attempts = 0;
      this.unauthorized = 0;
      this.set({ status: 'online', error: null, closeCode: null, lastOnlineAt: clock.now() });
      await this.poll(rpc);
      this.schedulePoll(rpc, gen);
    } catch (e) {
      if (gen === this.generation && !rpc.closed) {
        const kind = e instanceof RpcError ? e.kind : null;
        if (kind === 'forbidden' || kind === 'revoked') {
          this.running = false;
          this.set({ status: 'revoked', error: (e as Error).message });
        } else this.set({ error: (e as Error).message });
        rpc.close();
      }
    }
  }

  private schedulePoll(rpc: RpcClient, gen: number): void {
    const { clock } = this.o.platform;
    this.pollTimer = clock.setTimeout(() => {
      this.pollTimer = null;
      if (gen !== this.generation || rpc.closed) return;
      void this.poll(rpc).finally(() => {
        if (gen === this.generation && !rpc.closed) this.schedulePoll(rpc, gen);
      });
    }, this.o.pollMs ?? REVIEW_POLL_MS);
  }

  private async poll(rpc: RpcClient): Promise<void> {
    const preview = this.state.review?.preview.id ?? this.state.record.preview.id;
    try {
      const r = await rpc.request<{ draft: AnnotationDraft | null; drafts?: AnnotationDraft[] }>('browser.annotations', { preview });
      if (rpc !== this.rpc) return;
      const draft = r?.draft ?? null;
      // Newer hosts list every draft of this reviewer (`drafts`); sent ones are those not `open`.
      const all = Array.isArray(r?.drafts) ? r.drafts : draft ? [draft] : [];
      for (const d of all) {
        if (!d || typeof d.id !== 'string' || d.state === 'open') continue;
        this.sentDrafts.set(d.id, d);
        if (!(this.state.record.sent ?? []).includes(d.id)) await this.rememberSent(d);
      }
      // Older hosts: read the state of sent drafts that are not delivered yet, one by one.
      if (!Array.isArray(r?.drafts)) {
        for (const id of this.state.record.sent ?? []) {
          const known = this.sentDrafts.get(id);
          if (known && TERMINAL.has(known.state)) continue;
          if (draft && draft.id === id) continue;
          try {
            const s = await rpc.request<{ draft: AnnotationDraft | null }>('browser.annotations', { draft: id });
            if (s?.draft) this.sentDrafts.set(id, s.draft);
          } catch {
            // gone or not readable: keep the last known state
          }
        }
      }
      if (rpc !== this.rpc) return;
      this.set({ draft: draft && draft.state === 'open' ? draft : null, sent: this.sentList() });
    } catch {
      // the next poll tries again
    }
  }

  private onDisconnected(err: unknown): void {
    this.rpc = null;
    if (this.pollTimer !== null) this.o.platform.clock.clearTimeout(this.pollTimer);
    this.pollTimer = null;
    if (!this.running) return;
    const ce = err instanceof ChannelError ? err : null;
    const closeCode = ce?.closeCode ?? null;
    if (ce?.code === 'unsupported_version') {
      this.running = false;
      this.set({ status: 'incompatible', error: ce.message, closeCode });
      return;
    }
    if (ce?.code === 'unauthorized') {
      // Unauthenticated hint; the third in a row counts as revoked (spec 16 §4.4).
      if (++this.unauthorized >= 3 || ce.reason?.startsWith('ticket_')) {
        this.running = false;
        this.set({ status: ce.reason?.startsWith('ticket_') ? 'ticket_expired' : 'revoked', error: ce.message, closeCode });
        return;
      }
    }
    if (this.state.status === 'revoked') return;
    const { clock, random } = this.o.platform;
    const min = this.o.backoffMinMs ?? 1_000;
    const max = this.o.backoffMaxMs ?? 30_000;
    const base = Math.min(max, min * 2 ** this.attempts++);
    const r = new DataView(random(4).buffer).getUint32(0) / 2 ** 32;
    const delay = Math.floor(base / 2 + (r * base) / 2);
    const gen = this.generation;
    this.retryTimer = clock.setTimeout(() => {
      this.retryTimer = null;
      if (this.running && gen === this.generation) void this.connect();
    }, delay);
    this.set({ status: ce?.code === 'unauthorized' ? 'unauthorized' : 'offline', error: err ? (err as Error).message : 'disconnected', closeCode, nextRetryAt: clock.now() + delay });
  }

  private clearTimers(): void {
    const { clock } = this.o.platform;
    if (this.retryTimer !== null) clock.clearTimeout(this.retryTimer);
    if (this.pollTimer !== null) clock.clearTimeout(this.pollTimer);
    this.retryTimer = this.pollTimer = null;
  }
}

/** Check the fields the screen relies on; tolerate extras (additive API). */
export function normalizeReviewInfo(raw: unknown): ReviewInfo {
  const r = raw as Partial<ReviewInfo> | null;
  if (!r || typeof r !== 'object' || !r.preview || typeof r.preview.id !== 'string' || typeof r.preview.url !== 'string') {
    throw new Error('review.info: malformed result');
  }
  return {
    ...r,
    preview: { ...r.preview },
    owner: typeof r.owner === 'string' ? r.owner : '',
    notes: r.notes === 'held' ? 'held' : 'direct',
    sid: typeof r.sid === 'string' ? r.sid.toLowerCase() : '',
  } as ReviewInfo;
}

/** The preview's dev-server authority (`localhost:5173`) from `review.info`. */
export function previewAuthority(info: ReviewInfo): string | null {
  if (typeof info.preview.port === 'number' && info.preview.port > 0 && info.preview.port < 65536) return `localhost:${info.preview.port}`;
  try {
    const u = new URL(info.preview.url);
    return u.host || null;
  } catch {
    return null;
  }
}

/** The path the visible frame opens first (`review.info` path, else the preview URL's path). */
export function previewPath(info: ReviewInfo): string {
  const p = info.preview.path;
  if (typeof p === 'string' && p.startsWith('/')) return p;
  try {
    const u = new URL(info.preview.url);
    return `${u.pathname}${u.search}` || '/';
  } catch {
    return '/';
  }
}
