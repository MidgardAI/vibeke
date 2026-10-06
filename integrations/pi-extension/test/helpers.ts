import * as net from "node:net";
import { mkdtempSync, rmSync, existsSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createExtension, type Handle, type Options } from "../src/index.ts";

export const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

export async function waitFor(cond: () => boolean, ms = 3000, what = "condition"): Promise<void> {
  const end = Date.now() + ms;
  while (!cond()) {
    if (Date.now() > end) throw new Error(`timeout waiting for ${what}`);
    await sleep(5);
  }
}

export interface Msg {
  conn: number;
  msg: any;
}

/** Fake Vibeke server: acks hello/signal, parks adapter.gate until the test answers. */
export class FakeServer {
  dir = mkdtempSync(join(tmpdir(), "vk-pi-"));
  path = join(this.dir, "vibeke.sock");
  msgs: Msg[] = [];
  conns: net.Socket[] = [];
  closedConns = new Set<number>();
  gates: { conn: number; id: number; params: any }[] = [];
  /** Methods the fake leaves unanswered (to exercise client-side retries). */
  noReply = new Set<string>();
  private server: net.Server | null = null;

  async listen(): Promise<void> {
    if (existsSync(this.path)) rmSync(this.path);
    this.server = net.createServer((sock) => {
      const conn = this.conns.push(sock) - 1;
      let buf = "";
      sock.setEncoding("utf8"); // decode across chunk boundaries (multi-byte prompts)
      sock.on("error", () => {});
      sock.on("close", () => this.closedConns.add(conn));
      sock.on("data", (d) => {
        buf += d.toString();
        let i: number;
        while ((i = buf.indexOf("\n")) >= 0) {
          const line = buf.slice(0, i);
          buf = buf.slice(i + 1);
          const msg = JSON.parse(line);
          this.msgs.push({ conn, msg });
          if (msg.method === "adapter.gate") this.gates.push({ conn, id: msg.id, params: msg.params });
          else if (!this.noReply.has(msg.method))
            sock.write(JSON.stringify({ jsonrpc: "2.0", id: msg.id, result: {} }) + "\n");
        }
      });
    });
    await new Promise<void>((r) => this.server!.listen(this.path, r));
  }

  /** Stop listening and drop every connection (simulates a server kill). */
  async kill(): Promise<void> {
    for (const c of this.conns) c.destroy();
    await new Promise<void>((r) => (this.server ? this.server.close(() => r()) : r()));
    this.server = null;
  }

  respondGate(index: number, decision: { value: unknown } | null, extra: Record<string, unknown> = {}): void {
    const g = this.gates[index];
    this.conns[g.conn].write(JSON.stringify({ jsonrpc: "2.0", id: g.id, result: { decision, ...extra } }) + "\n");
  }

  /** Drop one gate's connection (as a server restart or network blip would). */
  dropGate(index: number): void {
    this.conns[this.gates[index].conn].destroy();
  }

  /** Messages of `method`, with the connection they arrived on. */
  calls(method: string): Msg[] {
    return this.msgs.filter((m) => m.msg.method === method);
  }

  /** The connection's `client.hello` params (token etc.). */
  hello(conn: number): any {
    return this.msgs.find((m) => m.conn === conn && m.msg.method === "client.hello")?.msg.params;
  }

  signals(conn?: number): { event: string; payload: any; harness: string }[] {
    return this.msgs
      .filter((m) => m.msg.method === "adapter.signal" && (conn === undefined || m.conn === conn))
      .map((m) => ({ event: m.msg.params.event, payload: m.msg.params.payload, harness: m.msg.params.harness }));
  }

  events(): string[] {
    return this.signals().map((s) => s.event);
  }

  cleanup(): void {
    rmSync(this.dir, { recursive: true, force: true });
  }
}

export class FakeHost {
  handlers = new Map<string, ((e: any, c: any) => any)[]>();
  version = "9.9.9";
  on(name: string, h: (e: any, c: any) => any) {
    if (!this.handlers.has(name)) this.handlers.set(name, []);
    this.handlers.get(name)!.push(h);
  }
  /** Calls every handler and returns the first result. */
  async emit(name: string, event: any, ctx: any): Promise<any> {
    let first: any;
    for (const h of this.handlers.get(name) ?? []) {
      const r = await h(event, ctx);
      if (first === undefined) first = r;
    }
    return first;
  }
  /** Synchronous variant for hot-path timing. */
  emitSync(name: string, event: any, ctx: any): any {
    let first: any;
    for (const h of this.handlers.get(name) ?? []) {
      const r = h(event, ctx);
      if (first === undefined) first = r;
    }
    return first;
  }
}

export function makeCtx(over: Record<string, unknown> = {}) {
  return {
    cwd: "/work/proj",
    mode: "tui",
    model: { id: "m-1" },
    sessionManager: { getSessionFile: () => "/home/u/.pi/agent/sessions/s1.jsonl", getSessionId: () => "sess-1" },
    ui: undefined as any,
    ...over,
  };
}

export function activeEnv(server: FakeServer): Record<string, string> {
  return { VIBEKE: "1", VIBEKE_SOCKET: server.path, VIBEKE_PANE_TOKEN: "tok" };
}

export function setup(server: FakeServer, host: "pi" | "omp", extra: Partial<Options> = {}) {
  const pi = new FakeHost();
  const h = createExtension(pi as any, {
    env: activeEnv(server),
    host,
    clientOverrides: { backoffMinMs: 10, backoffMaxMs: 40, ...extra.clientOverrides },
    ...extra,
  }) as Handle;
  return { pi, h };
}

/** Fake shared uiContext. Native dialogs stay open until the test settles them or the signal aborts. */
export function fakeUi() {
  const calls: { method: string; args: any[]; signal?: AbortSignal; aborts: number; resolve: (v: any) => void }[] = [];
  const mk = (method: string) =>
    function (this: unknown, ...args: any[]) {
      const opts = args[2] ?? {};
      return new Promise((resolve) => {
        const rec = { method, args, signal: opts.signal as AbortSignal | undefined, aborts: 0, resolve };
        if (rec.signal?.aborted) resolve(undefined);
        rec.signal?.addEventListener("abort", () => {
          rec.aborts++;
          resolve(undefined); // pi resolves an aborted dialog with its default
        });
        calls.push(rec);
      });
    };
  const ui: any = { confirm: mk("confirm"), select: mk("select"), input: mk("input"), notify() {} };
  return { ui, calls };
}
