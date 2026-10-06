// JSON-RPC 2.0 client to Vibeke over a Unix socket (DESIGN §2, PROTOCOL.md).
// One persistent connection for signals; one short-lived connection per dialog gate.
// Nothing here ever blocks the host: signal() only serializes and queues.
import * as net from "node:net";

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

export interface GateHandle {
  /** `{value}` when Vibeke answered, `null` for "no Vibeke answer" (or any failure). */
  answer: Promise<{ value: unknown } | null>;
  close(): void;
}

function lineReader(onLine: (line: string) => void): (chunk: Buffer | string) => void {
  let buf = "";
  return (chunk) => {
    buf += chunk.toString();
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
  /** Number of times the queue overflowed and was cleared (diagnostics/tests). */
  dropped = 0;
  connects = 0;

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
    s.on("data", lineReader(() => {})); // responses are acks; ignored
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
    } catch {
      /* ignore */
    }
  }

  /**
   * Blocking long-poll call on its own connection (adapter.gate). The server
   * treats a closed connection as "resolved elsewhere".
   */
  gate(event: string, payload: Record<string, unknown>): GateHandle {
    let settle!: (v: { value: unknown } | null) => void;
    let done = false;
    const answer = new Promise<{ value: unknown } | null>((r) => {
      settle = (v) => {
        if (!done) {
          done = true;
          r(v);
        }
      };
    });
    let s: net.Socket | null = null;
    try {
      s = net.createConnection(this.o.socketPath);
    } catch {
      settle(null);
      return { answer, close: () => {} };
    }
    s.unref();
    s.on("error", () => settle(null));
    s.on("close", () => settle(null));
    const gateId = 2;
    s.on(
      "data",
      lineReader((line) => {
        try {
          const m = JSON.parse(line);
          if (m.id !== gateId) return;
          const d = m.result?.decision;
          settle(d && typeof d === "object" && "value" in d ? { value: d.value } : null);
        } catch {
          settle(null);
        }
      }),
    );
    s.on("connect", () => {
      try {
        s!.write(
          JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "client.hello",
            params: { client: "vibeke-pi-extension", kind: "agent", token: this.o.token, version: this.o.version },
          }) + "\n",
        );
        s!.write(
          JSON.stringify({
            jsonrpc: "2.0",
            id: gateId,
            method: "adapter.gate",
            params: { harness: this.harness, event, payload },
          }) + "\n",
        );
      } catch {
        settle(null);
      }
    });
    return {
      answer,
      close: () => {
        settle(null);
        try {
          s?.destroy();
        } catch {
          /* ignore */
        }
      },
    };
  }
}
