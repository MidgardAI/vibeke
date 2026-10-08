// JSON-RPC 2.0 client to Vibeke over a Unix socket (DESIGN §2, PROTOCOL.md).
// One persistent connection for signals and delivery acks; one connection per dialog gate,
// reconnected (same payload, same dialog_id) if it drops while the dialog is still pending.
// Nothing here ever blocks the host: signal() only serializes and queues.
import * as net from "node:net";
import { StringDecoder } from "node:string_decoder";

export interface ClientOptions {
  socketPath: string;
  token: string;
  harness: string | (() => string);
  version: string;
  /** Builds the live-state snapshot; called on every (re)connect. */
  snapshot: () => Record<string, unknown>;
  queueCap?: number;
  backoffMinMs?: number;
  backoffMaxMs?: number;
  /** Failed attempts per burst before idling until the next signal. */
  maxTries?: number;
}

export interface GateAnswer {
  value: unknown;
  /** The server's interaction id and delivery key (`"<interaction>:<decision_rev>"`), for the ack. */
  interaction?: string;
  idempotencyKey?: string;
}

export interface GateHandle {
  /** The decision when Vibeke answered, `null` for "no Vibeke answer" (or a definitive failure). */
  answer: Promise<GateAnswer | null>;
  close(): void;
}

const MAX_PENDING_ACKS = 100;

/** Carries out one control request (`models`, `set_model`, `commands`); throws to refuse it. */
export type ControlHandler = (op: string, params: Record<string, unknown>) => Promise<unknown>;

function lineReader(onLine: (line: string) => void): (chunk: Buffer | string) => void {
  let buf = "";
  const dec = new StringDecoder("utf8"); // a UTF-8 sequence may span chunks
  return (chunk) => {
    buf += typeof chunk === "string" ? chunk : dec.write(chunk);
    let i: number;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (line.trim()) onLine(line);
    }
  };
}

export class VibekeClient {
  private sock: net.Socket | null = null;
  private ready = false;
  private blocked = false;
  private connecting = false;
  private closed = false;
  private everReady = false;
  private queue: string[] = [];
  private lastSeq = 0;
  private rpcId = 0;
  private tries = 0;
  private lastBurstEnd = 0;
  private timer: ReturnType<typeof setTimeout> | null = null;
  /** Delivery acks awaiting the server's response, by JSON-RPC id (re-sent after a reconnect). */
  private acks = new Map<number, string>();
  /** Number of times the queue overflowed and was cleared (diagnostics/tests). */
  dropped = 0;
  connects = 0;
  /** Gate connections re-established after a drop (diagnostics/tests). */
  gateReconnects = 0;
  /** Control requests carried out (diagnostics/tests). */
  controlHandled = 0;
  private controlStarted = false;
  private controlSock: net.Socket | null = null;

  private readonly cap: number;
  private readonly minMs: number;
  private readonly maxMs: number;
  private readonly maxTries: number;

  constructor(private readonly o: ClientOptions) {
    this.cap = o.queueCap ?? 500;
    this.minMs = o.backoffMinMs ?? 50;
    this.maxMs = o.backoffMaxMs ?? 2000;
    this.maxTries = o.maxTries ?? 5;
  }

  /** Monotonic `Date.now()*1000 + n`. */
  nextSeq(): number {
    const base = Date.now() * 1000;
    this.lastSeq = base > this.lastSeq ? base : this.lastSeq + 1;
    return this.lastSeq;
  }

  private get harness(): string {
    return typeof this.o.harness === "function" ? this.o.harness() : this.o.harness;
  }

  /** Begin connecting without sending a signal (the first connect sends the Snapshot). */
  start(): void {
    this.ensureConnecting();
  }

  private frame(method: string, params: unknown): string {
    return JSON.stringify({ jsonrpc: "2.0", id: ++this.rpcId, method, params }) + "\n";
  }

  /** Fire-and-forget. Never throws, never awaits the server. Returns the seq used. */
  signal(event: string, payload: Record<string, unknown> = {}): number {
    const seq = this.nextSeq();
    if (this.closed) return seq;
    try {
      const line = this.frame("adapter.signal", {
        harness: this.harness,
        event,
        payload: { ...payload, seq },
      });
      if (this.queue.length >= this.cap) {
        this.queue.length = 0; // the snapshot on reconnect repairs state; the queue is not a history
        this.dropped++;
      }
      this.queue.push(line);
      if (this.ready) this.pump();
      else this.ensureConnecting();
    } catch {
      /* never affect the host */
    }
    return seq;
  }

  private ensureConnecting(): void {
    if (this.closed || this.connecting || this.ready || this.timer) return;
    if (this.tries >= this.maxTries) {
      // Burst exhausted: allow a new burst once the max backoff has elapsed.
      if (Date.now() - this.lastBurstEnd < this.maxMs) return;
      this.tries = 0;
    }
    this.connect();
  }

  private connect(): void {
    this.connecting = true;
    let s: net.Socket;
    try {
      s = net.createConnection(this.o.socketPath);
    } catch {
      this.connecting = false;
      this.scheduleRetry();
      return;
    }
    this.sock = s;
    s.unref();
    s.setNoDelay?.(true);
    s.on("error", () => {});
    s.on(
      "data",
      lineReader((line) => {
        // Signal responses are plain acks; only delivery-ack responses matter (any answer,
        // success or a definitive refusal, ends the retry).
        try {
          const m = JSON.parse(line);
          if (typeof m?.id === "number") this.acks.delete(m.id);
        } catch {
          /* ignore */
        }
      }),
    );
    s.on("connect", () => {
      this.connecting = false;
      this.tries = 0;
      this.connects++;
      try {
        s.write(
          this.frame("client.hello", {
            client: "vibeke-pi-extension",
            kind: "agent",
            token: this.o.token,
            version: this.o.version,
          }),
        );
        // The queue is only a history on the very first connect (nothing sent yet, nothing dropped):
        // then the snapshot is ordered *before* the queued signals so none is discarded server-side.
        // Otherwise (reconnect, or overflow) the queue is untrusted: the snapshot supersedes it.
        const trusted = this.connects === 1 && this.dropped === 0 && !this.everReady && this.queue.length > 0;
        let snap: Record<string, unknown> = {};
        try {
          snap = this.o.snapshot();
        } catch {
          /* send what we can */
        }
        let seq: number;
        if (trusted) {
          const m = /"seq":(\d+)/.exec(this.queue[0]);
          seq = m ? Number(m[1]) - 1 : this.nextSeq();
        } else {
          this.queue.length = 0;
          seq = this.nextSeq();
        }
        s.write(
          this.frame("adapter.signal", {
            harness: this.harness,
            event: "Snapshot",
            payload: { ...snap, seq },
          }),
        );
        // Acks not yet answered (lost with the previous connection) go out again.
        for (const line of this.acks.values()) s.write(line);
        this.everReady = true;
        this.ready = true;
        this.blocked = false;
        this.pump();
      } catch {
        s.destroy();
      }
    });
    s.on("drain", () => {
      this.blocked = false;
      this.pump();
    });
    s.on("close", () => {
      if (this.sock === s) this.sock = null;
      const wasReady = this.ready;
      this.ready = false;
      this.blocked = false;
      this.connecting = false;
      if (this.closed) return;
      if (wasReady) this.tries = 0;
      this.scheduleRetry();
    });
  }

  private scheduleRetry(): void {
    if (this.closed || this.timer) return;
    if (this.tries >= this.maxTries) {
      this.lastBurstEnd = Date.now();
      return;
    }
    const delay = Math.min(this.minMs * 2 ** this.tries, this.maxMs);
    this.tries++;
    this.timer = setTimeout(() => {
      this.timer = null;
      this.connect();
    }, delay);
    this.timer.unref?.();
  }

  private pump(): void {
    const s = this.sock;
    if (!s || !this.ready || this.blocked) return;
    try {
      while (this.queue.length) {
        const line = this.queue.shift() as string;
        if (!s.write(line)) {
          this.blocked = true;
          return;
        }
      }
    } catch {
      /* socket closing; reconnect repairs */
    }
  }

  /**
   * `adapter.delivery_ack` on the main (pane-token) connection: Vibeke's answer was applied to
   * the native dialog. Retried after reconnects until the server responds. Never throws.
   */
  deliveryAck(interaction: string, idempotencyKey: string): void {
    if (this.closed) return;
    try {
      const id = ++this.rpcId;
      const line =
        JSON.stringify({
          jsonrpc: "2.0",
          id,
          method: "adapter.delivery_ack",
          params: { interaction, idempotency_key: idempotencyKey, applied: true },
        }) + "\n";
      if (this.acks.size >= MAX_PENDING_ACKS) {
        const oldest = this.acks.keys().next().value;
        if (oldest !== undefined) this.acks.delete(oldest);
      }
      this.acks.set(id, line);
      if (this.ready && this.sock) this.sock.write(line);
      else this.ensureConnecting();
    } catch {
      /* never affect the host */
    }
  }

  /** Delivery acks still awaiting a server response (tests/diagnostics). */
  get pendingAcks(): number {
    return this.acks.size;
  }

  /** Resolve when the queue and socket buffer are drained, or after `timeoutMs`. */
  flush(timeoutMs: number): Promise<void> {
    const deadline = Date.now() + timeoutMs;
    return new Promise((resolve) => {
      const tick = () => {
        const drained = this.ready && this.queue.length === 0 && (this.sock?.writableLength ?? 0) === 0;
        if (drained || Date.now() >= deadline || this.closed) return resolve();
        setTimeout(tick, 5);
      };
      tick();
    });
  }

  get connected(): boolean {
    return this.ready;
  }

  close(): void {
    this.closed = true;
    if (this.timer) clearTimeout(this.timer);
    this.timer = null;
    this.queue.length = 0;
    try {
      this.sock?.destroy();
      this.controlSock?.destroy();
    } catch {
      /* ignore */
    }
  }

  /**
   * Control channel (PROTOCOL.md): long-poll `adapter.control {ops}` on its own connection and
   * answer each request with the next poll's `reply`. Idempotent. A server that does not know
   * the method (an error, or a result without `request`) ends the loop for good; a dropped
   * connection reconnects with backoff.
   */
  control(ops: string[], handle: ControlHandler): void {
    if (this.controlStarted || this.closed || ops.length === 0) return;
    this.controlStarted = true;
    let tries = 0;
    let reply: Record<string, unknown> | undefined;
    const retry = () => {
      if (this.closed) return;
      const delay = Math.min(this.minMs * 2 ** Math.min(tries, 16), this.maxMs);
      tries++;
      const t = setTimeout(connect, delay);
      t.unref?.();
    };
    const connect = () => {
      if (this.closed) return;
      let sock: net.Socket;
      try {
        sock = net.createConnection(this.o.socketPath);
      } catch {
        retry();
        return;
      }
      this.controlSock = sock;
      sock.unref();
      sock.on("error", () => {});
      let id = 1;
      let sentAt = 0;
      let stopped = false;
      const poll = () => {
        if (this.closed || sock.destroyed) return;
        id++;
        sentAt = Date.now();
        const params: Record<string, unknown> = { harness: this.harness, ops };
        if (reply) params.reply = reply;
        reply = undefined;
        try {
          sock.write(JSON.stringify({ jsonrpc: "2.0", id, method: "adapter.control", params }) + "\n");
        } catch {
          sock.destroy();
        }
      };
      sock.on(
        "data",
        lineReader((line) => {
          let m: any;
          try {
            m = JSON.parse(line);
          } catch {
            return;
          }
          if (m?.id !== id) return;
          const r = m.result;
          if (m.error || !r || typeof r !== "object" || !("request" in r)) {
            stopped = true; // this server has no control channel
            sock.destroy();
            return;
          }
          tries = 0;
          const req = r.request;
          if (req && typeof req === "object" && typeof req.id === "string") {
            const params = req.params && typeof req.params === "object" ? req.params : {};
            Promise.resolve()
              .then(() => handle(String(req.op), params))
              .then(
                (result) => ({ id: req.id, ok: true, result: result ?? null }),
                (e) => ({ id: req.id, ok: false, error: String(e?.message ?? e).slice(0, 500) }),
              )
              .then((rep) => {
                this.controlHandled++;
                reply = rep;
                poll();
              });
          } else if (Date.now() - sentAt < 1000) {
            // An empty answer that came back at once: do not spin.
            const t = setTimeout(poll, 1000);
            t.unref?.();
          } else {
            poll();
          }
        }),
      );
      sock.on("connect", () => {
        try {
          sock.write(
            JSON.stringify({
              jsonrpc: "2.0",
              id: 1,
              method: "client.hello",
              params: { client: "vibeke-pi-extension", kind: "agent", token: this.o.token, version: this.o.version },
            }) + "\n",
          );
        } catch {
          sock.destroy();
          return;
        }
        poll();
      });
      sock.on("close", () => {
        if (this.controlSock === sock) this.controlSock = null;
        if (!stopped) retry();
      });
    };
    connect();
  }

  /**
   * Blocking long-poll call on its own connection (adapter.gate). The server treats a closed
   * connection as "resolved elsewhere" for that attempt; if the connection drops before a
   * response, the same request (same payload, so the same `dialog_id`) is issued again on a new
   * connection and the server re-attaches the interaction by its native ref. Stops on a
   * response or `close()`.
   */
  gate(event: string, payload: Record<string, unknown>): GateHandle {
    let settle!: (v: GateAnswer | null) => void;
    let done = false;
    const answer = new Promise<GateAnswer | null>((r) => {
      settle = (v) => {
        if (!done) {
          done = true;
          r(v);
        }
      };
    });
    let s: net.Socket | null = null;
    let timer: ReturnType<typeof setTimeout> | null = null;
    let tries = 0;
    let attempts = 0;
    const gateId = 2;
    const stop = () => {
      if (timer) clearTimeout(timer);
      timer = null;
      try {
        s?.destroy();
      } catch {
        /* ignore */
      }
      s = null;
    };
    const retry = () => {
      if (done || timer) return;
      if (this.closed) return settle(null);
      const delay = Math.min(this.minMs * 2 ** tries, this.maxMs);
      tries++;
      timer = setTimeout(() => {
        timer = null;
        attempt();
      }, delay);
      timer.unref?.();
    };
    const attempt = () => {
      if (done) return;
      if (this.closed) return settle(null);
      if (attempts++ > 0) this.gateReconnects++;
      let sock: net.Socket;
      try {
        sock = net.createConnection(this.o.socketPath);
      } catch {
        retry();
        return;
      }
      s = sock;
      sock.unref();
      sock.on("error", () => {});
      sock.on("close", () => {
        if (s === sock) s = null;
        retry(); // no-op once settled
      });
      sock.on(
        "data",
        lineReader((line) => {
          try {
            const m = JSON.parse(line);
            if (m.id !== gateId) return;
            const r = m.result;
            const d = r?.decision;
            if (d && typeof d === "object" && "value" in d) {
              settle({
                value: d.value,
                interaction: typeof r.interaction === "string" ? r.interaction : undefined,
                idempotencyKey: typeof r.idempotency_key === "string" ? r.idempotency_key : undefined,
              });
            } else {
              settle(null);
            }
          } catch {
            settle(null);
          }
          stop();
        }),
      );
      sock.on("connect", () => {
        try {
          sock.write(
            JSON.stringify({
              jsonrpc: "2.0",
              id: 1,
              method: "client.hello",
              params: { client: "vibeke-pi-extension", kind: "agent", token: this.o.token, version: this.o.version },
            }) + "\n",
          );
          sock.write(
            JSON.stringify({
              jsonrpc: "2.0",
              id: gateId,
              method: "adapter.gate",
              params: { harness: this.harness, event, payload },
            }) + "\n",
          );
        } catch {
          sock.destroy();
        }
      });
    };
    attempt();
    return {
      answer,
      close: () => {
        settle(null);
        stop();
      },
    };
  }
}
