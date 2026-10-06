// JSON-RPC 2.0 client over a Channel (spec 16 §5 liveness, §7.3 envelope and op_id rules).
//
// - Responses are matched by numeric id.
// - Mutating calls get a client-generated `op_id` (UUID v4) unless the caller supplied one.
// - A request whose response is lost (timeout or channel close) rejects with OutcomeUnknownError.
//   Nothing is ever retried automatically (§1.7): the UI refetches and asks the user.
// - Liveness: `ping` every 20 s; the channel is closed when nothing authenticated arrived for 60 s.

import { ChannelError, type MessageChannel } from './channel';
import type { Clock, TimerHandle } from './platform';

export interface RpcErrorObject {
  code: number;
  message: string;
  data?: { kind?: string; details?: unknown; retryable?: boolean; [k: string]: unknown };
}

/** The peer answered with a JSON-RPC error. The outcome is known: the call failed. */
export class RpcError extends Error {
  readonly code: number;
  /** Stable machine-readable kind (`data.kind`), e.g. `forbidden`, `stale`, `not_found`. */
  readonly kind: string;
  readonly data: RpcErrorObject['data'];
  constructor(readonly method: string, e: RpcErrorObject) {
    super(`${method}: ${e.message}`);
    this.name = 'RpcError';
    this.code = e.code;
    this.data = e.data;
    this.kind = typeof e.data?.kind === 'string' ? e.data.kind : e.message;
  }
}

/**
 * The request may or may not have taken effect (response lost to a timeout or a dropped
 * connection). Never retry automatically; refetch state and let the user decide.
 */
export class OutcomeUnknownError extends Error {
  constructor(
    readonly method: string,
    readonly mutating: boolean,
    readonly opId: string | undefined,
    readonly reason: 'timeout' | 'closed',
    override readonly cause?: unknown,
  ) {
    super(`${method}: outcome unknown (${reason})`);
    this.name = 'OutcomeUnknownError';
  }
}

export interface RequestOptions {
  /** Adds an `op_id` to params (spec 16 §7.3). */
  mutating?: boolean;
  /** Default: RpcOptions.defaultTimeoutMs. */
  timeoutMs?: number;
}

export interface RpcOptions {
  clock: Clock;
  random: (n: number) => Uint8Array;
  pingIntervalMs?: number;
  idleTimeoutMs?: number;
  defaultTimeoutMs?: number;
  /** Disable the ping/idle machinery (e.g. pairing connections). Default true. */
  keepalive?: boolean;
}

interface Pending {
  method: string;
  mutating: boolean;
  opId: string | undefined;
  timer: TimerHandle;
  resolve(v: unknown): void;
  reject(e: unknown): void;
}

type NotificationHandler = (params: unknown, method: string) => void;

export function uuidv4(random: (n: number) => Uint8Array): string {
  const b = random(16);
  b[6] = (b[6]! & 0x0f) | 0x40;
  b[8] = (b[8]! & 0x3f) | 0x80;
  const h = Array.from(b, (x) => x.toString(16).padStart(2, '0')).join('');
  return `${h.slice(0, 8)}-${h.slice(8, 12)}-${h.slice(12, 16)}-${h.slice(16, 20)}-${h.slice(20)}`;
}

export class RpcClient {
  private nextId = 1;
  private readonly pending = new Map<number, Pending>();
  private readonly handlers = new Map<string, Set<NotificationHandler>>();
  private readonly clock: Clock;
  private readonly opts: Required<Omit<RpcOptions, 'clock' | 'random'>>;
  private lastReceivedAt: number;
  private pingTimer: TimerHandle | null = null;
  private idleTimer: TimerHandle | null = null;
  private readonly offs: (() => void)[];

  constructor(
    private readonly channel: MessageChannel,
    private readonly o: RpcOptions,
  ) {
    this.clock = o.clock;
    this.opts = {
      pingIntervalMs: o.pingIntervalMs ?? 20_000,
      idleTimeoutMs: o.idleTimeoutMs ?? 60_000,
      defaultTimeoutMs: o.defaultTimeoutMs ?? 30_000,
      keepalive: o.keepalive ?? true,
    };
    this.lastReceivedAt = this.clock.now();
    this.offs = [channel.onMessage((m) => this.onMessage(m)), channel.onClose((e) => this.onClosed(e))];
    if (this.opts.keepalive && !channel.closed) {
      this.schedulePing();
      this.scheduleIdle(this.opts.idleTimeoutMs);
    }
  }

  get closed(): boolean {
    return this.channel.closed;
  }

  request<T = unknown>(method: string, params: Record<string, unknown> = {}, ro: RequestOptions = {}): Promise<T> {
    const mutating = ro.mutating ?? false;
    let opId: string | undefined;
    if (mutating) {
      opId = typeof params.op_id === 'string' ? params.op_id : uuidv4(this.o.random);
      params = { ...params, op_id: opId };
    }
    if (this.channel.closed) {
      return Promise.reject(new OutcomeUnknownError(method, mutating, opId, 'closed'));
    }
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      const timer = this.clock.setTimeout(() => {
        if (this.pending.delete(id)) reject(new OutcomeUnknownError(method, mutating, opId, 'timeout'));
      }, ro.timeoutMs ?? this.opts.defaultTimeoutMs);
      this.pending.set(id, { method, mutating, opId, timer, resolve: resolve as (v: unknown) => void, reject });
      try {
        this.channel.send({ jsonrpc: '2.0', id, method, params });
      } catch (e) {
        // Nothing was sent if encryption/serialization threw before the socket write, but a
        // closed socket mid-write is ambiguous; report the safe answer.
        this.clock.clearTimeout(timer);
        this.pending.delete(id);
        reject(e instanceof ChannelError && e.code === 'too_large' ? e : new OutcomeUnknownError(method, mutating, opId, 'closed', e));
      }
    });
  }

  /** Fire-and-forget notification to the host. */
  notify(method: string, params: Record<string, unknown> = {}): void {
    if (!this.channel.closed) this.channel.send({ jsonrpc: '2.0', method, params });
  }

  /** Subscribe to notifications by method name, or `*` for all. */
  on(method: string, cb: NotificationHandler): () => void {
    let set = this.handlers.get(method);
    if (!set) this.handlers.set(method, (set = new Set()));
    set.add(cb);
    return () => set.delete(cb);
  }

  close(): void {
    this.channel.close();
  }

  private onMessage(m: unknown): void {
    this.lastReceivedAt = this.clock.now();
    if (typeof m !== 'object' || m === null) return;
    const msg = m as { id?: unknown; method?: unknown; params?: unknown; result?: unknown; error?: RpcErrorObject };
    if (typeof msg.method === 'string') {
      if (msg.id !== undefined && msg.id !== null) {
        // Host→device requests are not part of the API; answer politely.
        this.channel.send({ jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: 'method not found' } });
        return;
      }
      for (const key of [msg.method, '*']) {
        for (const cb of [...(this.handlers.get(key) ?? [])]) cb(msg.params, msg.method);
      }
      return;
    }
    if (typeof msg.id !== 'number') return;
    const p = this.pending.get(msg.id);
    if (!p) return; // late response after timeout, or unknown id
    this.pending.delete(msg.id);
    this.clock.clearTimeout(p.timer);
    if (msg.error) p.reject(new RpcError(p.method, msg.error));
    else p.resolve(msg.result);
  }

  private onClosed(err: ChannelError | null): void {
    for (const off of this.offs) off();
    if (this.pingTimer !== null) this.clock.clearTimeout(this.pingTimer);
    if (this.idleTimer !== null) this.clock.clearTimeout(this.idleTimer);
    const pending = [...this.pending.values()];
    this.pending.clear();
    for (const p of pending) {
      this.clock.clearTimeout(p.timer);
      p.reject(new OutcomeUnknownError(p.method, p.mutating, p.opId, 'closed', err));
    }
  }

  private schedulePing(): void {
    this.pingTimer = this.clock.setTimeout(() => {
      this.pingTimer = null;
      if (this.channel.closed) return;
      this.request('ping', {}, { timeoutMs: this.opts.idleTimeoutMs }).catch(() => {});
      this.schedulePing();
    }, this.opts.pingIntervalMs);
  }

  private scheduleIdle(ms: number): void {
    this.idleTimer = this.clock.setTimeout(() => {
      this.idleTimer = null;
      if (this.channel.closed) return;
      const idle = this.clock.now() - this.lastReceivedAt;
      if (idle >= this.opts.idleTimeoutMs) {
        this.channel.close(new ChannelError('timeout', `nothing received for ${idle} ms`));
      } else {
        this.scheduleIdle(this.opts.idleTimeoutMs - idle);
      }
    }, ms);
  }
}
