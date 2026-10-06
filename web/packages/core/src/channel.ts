// The vibeke-e2e/1 channel over an injected Socket (spec 16 §4.2, §4.4, §5), initiator side.
//
//   text   hello (= Noise prologue)
//   binary Noise message 1 (empty payload)
//   binary Noise message 2 (payload: host info JSON)
//   binary transport frames, both directions (chunked JSON-RPC)
//
// Before the handshake completes the peer (or the relay) may send one plaintext JSON error frame,
// e.g. {"error":"unauthorized"}, then close. Those are surfaced as ChannelError but are NOT
// authenticated: a relay can forge them (see `ChannelError.authenticated`).

import { helloBytes, helloText, type Hello } from './hello';
import { Initiator, NoiseError, type Session } from './noise';
import type { Clock, Socket } from './platform';

export type ChannelErrorCode =
  | 'unauthorized' // plaintext {"error":"unauthorized"} (unauthenticated hint)
  | 'unsupported_version' // plaintext {"error":"unsupported_version"}
  | 'remote_error' // other plaintext {"error":...}
  | 'handshake' // Noise handshake failed (wrong host key, wrong psk, altered prologue)
  | 'decrypt' // transport frame failed authentication (tamper, replay, reorder)
  | 'protocol' // framing / message-type violation
  | 'too_large'
  | 'timeout'
  | 'closed'; // socket closed (relay close codes are untrusted hints)

export class ChannelError extends Error {
  constructor(
    readonly code: ChannelErrorCode,
    message: string,
    readonly opts: {
      /** WebSocket close code, when the socket closed. */
      closeCode?: number;
      /** Parsed plaintext error frame. */
      remote?: Record<string, unknown>;
      /** True only for errors derived from authenticated data. */
      authenticated?: boolean;
      cause?: unknown;
    } = {},
  ) {
    super(message);
    this.name = 'ChannelError';
  }
  get closeCode(): number | undefined {
    return this.opts.closeCode;
  }
  get authenticated(): boolean {
    return this.opts.authenticated ?? false;
  }
}

/** Relay close codes (spec 16 §6.1), untrusted. */
export const RELAY_CLOSE: Record<number, string> = {
  4400: 'bad_request',
  4401: 'unauthorized',
  4404: 'host_offline',
  4408: 'accept_timeout',
  4409: 'replaced',
  4413: 'too_large',
  4429: 'rate_limited',
  4503: 'draining',
};

/** Host info: the payload of Noise message 2. */
export interface HostHello {
  v: number;
  host_name: string;
  gateway_version: string;
  server_version: string;
  [k: string]: unknown;
}

export interface ChannelOptions {
  socket: Socket;
  hello: Hello;
  /** Pinned host static public key. */
  hostKey: Uint8Array;
  /** Device static private key. */
  devicePrivate: Uint8Array;
  /** Pairing secret (IKpsk2). Required iff hello.mode === 'pair'. */
  psk?: Uint8Array;
  clock: Clock;
  random?: (n: number) => Uint8Array;
  /** Fixed ephemeral key (tests only). */
  ephemeral?: Uint8Array;
  /** Whole-connect deadline: socket open + handshake. Default 15 s. */
  timeoutMs?: number;
}

/** The surface RpcClient needs; lets tests substitute a fake. */
export interface MessageChannel {
  send(msg: unknown): void;
  onMessage(cb: (msg: unknown) => void): () => void;
  onClose(cb: (err: ChannelError | null) => void): () => void;
  close(err?: ChannelError): void;
  readonly closed: boolean;
}

/** WebSocket close codes we send. 1000 normal; 4000-range are app-level (end-to-end) reasons. */
const CLOSE_NORMAL = 1000;
const CLOSE_PROTOCOL = 4002;

export class Channel implements MessageChannel {
  private readonly messageListeners = new Set<(msg: unknown) => void>();
  private readonly closeListeners = new Set<(err: ChannelError | null) => void>();
  /** Messages that arrived before anyone listened. */
  private backlog: unknown[] = [];
  private partialTimer: unknown = null;
  private closeReason: ChannelError | null | undefined = undefined;
  /** Time of the last authenticated frame (for liveness). */
  lastReceivedAt: number;

  private constructor(
    private readonly socket: Socket,
    private readonly session: Session,
    readonly host: HostHello,
    private readonly clock: Clock,
  ) {
    this.lastReceivedAt = clock.now();
    socket.onmessage = (data) => this.onFrame(data);
    socket.onclose = (code, reason) =>
      this.finish(
        new ChannelError('closed', `socket closed: ${code} ${RELAY_CLOSE[code] ?? reason}`.trim(), { closeCode: code }),
        false,
      );
  }

  /** Open the socket, send the hello and run the Noise handshake as initiator. */
  static connect(o: ChannelOptions): Promise<Channel> {
    const { socket, clock } = o;
    if ((o.hello.mode === 'pair') !== (o.psk !== undefined)) {
      return Promise.reject(new ChannelError('protocol', 'psk must be given exactly in pair mode'));
    }
    return new Promise<Channel>((resolve, reject) => {
      let settled = false;
      let initiator: Initiator | null = null;
      const fail = (err: ChannelError) => {
        if (settled) return;
        settled = true;
        clock.clearTimeout(timer);
        socket.onopen = socket.onmessage = socket.onclose = null;
        if (socket.state !== 'closed') socket.close(CLOSE_PROTOCOL, err.code);
        reject(err);
      };
      const timer = clock.setTimeout(
        () => fail(new ChannelError('timeout', 'connect/handshake timed out')),
        o.timeoutMs ?? 15_000,
      );

      const start = () => {
        try {
          const prologue = helloBytes(o.hello);
          initiator = new Initiator({
            prologue,
            localPrivate: o.devicePrivate,
            remotePublic: o.hostKey,
            psk: o.psk ?? null,
            ephemeral: o.ephemeral,
            random: o.random,
          });
          socket.send(helloText(o.hello));
          socket.send(initiator.writeFirst());
        } catch (e) {
          fail(new ChannelError('handshake', `handshake setup failed: ${(e as Error).message}`, { cause: e }));
        }
      };

      socket.onopen = start;
      socket.onclose = (code, reason) =>
        fail(
          new ChannelError('closed', `socket closed during handshake: ${code} ${RELAY_CLOSE[code] ?? reason}`.trim(), {
            closeCode: code,
          }),
        );
      socket.onmessage = (data) => {
        if (settled) return;
        if (typeof data === 'string') return fail(plaintextError(data));
        if (!initiator) return fail(new ChannelError('protocol', 'binary message before hello'));
        let payload: Uint8Array;
        let session: Session;
        try {
          ({ payload, session } = initiator.readSecond(data, clock));
        } catch (e) {
          return fail(new ChannelError('handshake', `handshake failed: ${(e as Error).message}`, { cause: e }));
        }
        let host: HostHello;
        try {
          host = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(payload)) as HostHello;
          if (typeof host !== 'object' || host === null || typeof host.host_name !== 'string') throw new Error('shape');
        } catch {
          return fail(new ChannelError('protocol', 'bad host info payload', { authenticated: true }));
        }
        settled = true;
        clock.clearTimeout(timer);
        socket.onopen = null;
        resolve(new Channel(socket, session, host, clock));
      };

      if (socket.state === 'open') start();
      else if (socket.state === 'closed') fail(new ChannelError('closed', 'socket already closed'));
    });
  }

  get closed(): boolean {
    return this.closeReason !== undefined;
  }

  /** Serialize to JSON and send as one or more encrypted binary frames. */
  send(msg: unknown): void {
    if (this.closed) throw new ChannelError('closed', 'channel closed');
    let frames: Uint8Array[];
    try {
      frames = this.session.encrypt(new TextEncoder().encode(JSON.stringify(msg)));
    } catch (e) {
      if (e instanceof NoiseError && e.kind === 'too_large') throw new ChannelError('too_large', 'message too large');
      throw e;
    }
    for (const f of frames) this.socket.send(f);
  }

  /** Subscribe to decrypted application messages. Buffered messages are delivered immediately. */
  onMessage(cb: (msg: unknown) => void): () => void {
    this.messageListeners.add(cb);
    if (this.backlog.length) {
      const pending = this.backlog;
      this.backlog = [];
      for (const m of pending) cb(m);
    }
    return () => this.messageListeners.delete(cb);
  }

  /** Called once with null (local close) or the error that ended the channel. */
  onClose(cb: (err: ChannelError | null) => void): () => void {
    if (this.closeReason !== undefined) {
      cb(this.closeReason);
      return () => {};
    }
    this.closeListeners.add(cb);
    return () => this.closeListeners.delete(cb);
  }

  /** Async iteration over messages; ends on close (throws the close error, if any). */
  async *messages(): AsyncGenerator<unknown, void, undefined> {
    const queue: unknown[] = [];
    let wake: (() => void) | null = null;
    const offMsg = this.onMessage((m) => {
      queue.push(m);
      wake?.();
    });
    const offClose = this.onClose(() => wake?.());
    try {
      for (;;) {
        while (queue.length) yield queue.shift();
        if (this.closed) {
          if (this.closeReason) throw this.closeReason;
          return;
        }
        await new Promise<void>((r) => (wake = r));
        wake = null;
      }
    } finally {
      offMsg();
      offClose();
    }
  }

  close(err?: ChannelError): void {
    this.finish(err ?? null, true);
  }

  private onFrame(data: string | Uint8Array): void {
    if (this.closed) return;
    if (typeof data === 'string') {
      return this.finish(new ChannelError('protocol', 'text message after handshake'), true);
    }
    let msg: Uint8Array | null;
    try {
      msg = this.session.decrypt(data);
    } catch (e) {
      const kind = e instanceof NoiseError ? e.kind : 'bad';
      const code: ChannelErrorCode = kind === 'decrypt' ? 'decrypt' : kind === 'too_large' ? 'too_large' : 'protocol';
      return this.finish(new ChannelError(code, (e as Error).message, { cause: e }), true);
    }
    this.lastReceivedAt = this.clock.now();
    this.armPartialTimer();
    if (msg === null) return;
    let value: unknown;
    try {
      value = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(msg));
    } catch {
      return this.finish(new ChannelError('protocol', 'message is not UTF-8 JSON', { authenticated: true }), true);
    }
    if (this.messageListeners.size === 0) {
      this.backlog.push(value);
      return;
    }
    for (const cb of [...this.messageListeners]) {
      try {
        cb(value);
      } catch (e) {
        // A listener bug must not tear down the channel; surface it asynchronously.
        queueMicrotask(() => {
          throw e;
        });
      }
    }
  }

  /** Close the connection when a partial message does not complete within 30 s. */
  private armPartialTimer(): void {
    if (this.partialTimer !== null) {
      this.clock.clearTimeout(this.partialTimer);
      this.partialTimer = null;
    }
    const left = this.session.partialDeadlineIn();
    if (left === null) return;
    this.partialTimer = this.clock.setTimeout(() => {
      this.partialTimer = null;
      try {
        this.session.checkDeadline();
        this.armPartialTimer();
      } catch (e) {
        this.finish(new ChannelError('timeout', (e as Error).message), true);
      }
    }, Math.max(0, left) + 1);
  }

  private finish(err: ChannelError | null, closeSocket: boolean): void {
    if (this.closeReason !== undefined) return;
    this.closeReason = err;
    if (this.partialTimer !== null) this.clock.clearTimeout(this.partialTimer);
    this.socket.onmessage = this.socket.onclose = null;
    if (closeSocket && this.socket.state !== 'closed') {
      this.socket.close(err ? CLOSE_PROTOCOL : CLOSE_NORMAL, err ? err.code : 'bye');
    }
    for (const cb of [...this.closeListeners]) cb(err);
    this.closeListeners.clear();
    this.messageListeners.clear();
  }
}

/** Turn a plaintext pre-handshake frame into a typed (unauthenticated) error. */
function plaintextError(text: string): ChannelError {
  let remote: Record<string, unknown> | undefined;
  try {
    const v = JSON.parse(text);
    if (typeof v === 'object' && v !== null) remote = v as Record<string, unknown>;
  } catch {
    /* not JSON */
  }
  const e = remote?.error;
  if (e === 'unauthorized') return new ChannelError('unauthorized', 'host refused this device', { remote });
  if (e === 'unsupported_version') return new ChannelError('unsupported_version', 'host speaks another protocol version', { remote });
  if (typeof e === 'string') return new ChannelError('remote_error', `host error: ${e}`, { remote });
  return new ChannelError('protocol', 'unexpected text message during handshake', { remote });
}
