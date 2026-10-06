// @vibeke/pi-extension: observe-only Vibeke integration for pi and omp.
// See DESIGN.md (normative) and PROTOCOL.md (wire contract).
// Runtime imports: node:* only. Host types are declared locally (types.ts).
import { randomUUID } from "node:crypto";
import { fileChangePath, preview, RATE_LIMIT_RE, redactInput } from "./describe.js";
import { VibekeClient } from "./protocol.js";
import type { HostApi, HostContext, UiContext } from "./types.js";
import { EXTENSION_VERSION } from "./version.js";

export { EXTENSION_VERSION };

export type HostKind = "pi" | "omp";

export interface Options {
  env?: Record<string, string | undefined>;
  host?: HostKind;
  /** Test hooks. */
  debounceMs?: number;
  clientOverrides?: Partial<ConstructorParameters<typeof VibekeClient>[0]>;
}

export interface Handle {
  client: VibekeClient;
  host: HostKind;
  /** Wrap `ui` now (idempotent). Exposed for tests. */
  ensureWrapped(ui: UiContext | undefined, ctx?: HostContext): boolean;
  wrapperEnabled(): boolean;
}

const CACHE_TTL_MS = 10 * 60 * 1000;
const WRAPPED_MARK = Symbol.for("vibeke.wrapped");
const DIALOG_METHODS = ["confirm", "select", "input"] as const;
type DialogMethod = (typeof DIALOG_METHODS)[number];

interface CachedCall {
  tool: string;
  input: unknown;
  at: number;
  started: boolean;
}

function detectHost(env: Record<string, string | undefined>): HostKind {
  const forced = env.VIBEKE_PI_HOST;
  if (forced === "pi" || forced === "omp") return forced;
  return (process.versions as Record<string, string | undefined>).bun ? "omp" : "pi";
}

/** Returns a handle when active, or undefined when inert. */
export function createExtension(pi: HostApi, opts: Options = {}): Handle | undefined {
  const env = opts.env ?? process.env;
  if (env.VIBEKE !== "1" || !env.VIBEKE_SOCKET || !env.VIBEKE_PANE_TOKEN) return undefined; // inert (also if only HERDR_ENV)

  let host: HostKind = opts.host ?? detectHost(env);
  const headlessOwner = env.VIBEKE_HEADLESS_OWNER === "1";
  const debounceMs = opts.debounceMs ?? 250;

  // ---- live state (for snapshots) ----
  let lastCtx: HostContext | undefined;
  let model: string | undefined;
  let turnIndex = 0;
  let streaming = false;
  let turnOpen = false;
  let promptSent = false;
  let compacting = false;
  let endTimer: ReturnType<typeof setTimeout> | null = null;
  let lastEnd: { stop_reason?: string; last_message?: string } = {};
  const calls = new Map<string, CachedCall>();
  const approvals = new Map<string, { tool: string; reason: unknown }>();

  const sessionId = () => safe(() => lastCtx?.sessionManager?.getSessionId?.()) ?? null;
  const sessionFile = () => safe(() => lastCtx?.sessionManager?.getSessionFile?.()) ?? null;

  const client = new VibekeClient({
    socketPath: env.VIBEKE_SOCKET,
    token: env.VIBEKE_PANE_TOKEN,
    harness: () => host,
    version: EXTENSION_VERSION,
    snapshot: () => ({
      session_id: sessionId(),
      session_file: sessionFile(),
      is_streaming: streaming,
      turn_index: turnIndex,
      model: model ?? null,
      host,
      host_version: hostVersion(),
      extension_version: EXTENSION_VERSION,
      pending_tool_calls: [...calls.entries()]
        .filter(([, c]) => c.started)
        .map(([call_id, c]) => ({ call_id, tool: c.tool, input: redactInput(c.input) })),
      open_approvals: [...approvals.entries()].map(([call_id, a]) => ({
        call_id,
        tool: a.tool,
        reason: a.reason,
      })),
    }),
    ...opts.clientOverrides,
  });
  // harness is fixed at construction; keep a mutable view for later host upgrades
  const hostVersion = () => (typeof pi.version === "string" ? pi.version : undefined);

  const HEADLESS_ALLOWED = new Set(["SessionStart", "ToolEnded", "Snapshot"]);
  function emit(event: string, payload: Record<string, unknown> = {}): void {
    if (headlessOwner && !HEADLESS_ALLOWED.has(event)) return;
    try {
      if (lastCtx?.ui) ensureWrapped(lastCtx.ui, lastCtx); // re-wrap after uiContext swap
      client.signal(event, payload);
    } catch {
      /* never affect the host */
    }
  }

  // ---- uiContext wrapper (DESIGN §4.1) ----
  const wrapped = new WeakSet<object>();
  let wrapperOk = true;

  function shapeOk(ui: UiContext): boolean {
    for (const m of DIALOG_METHODS) {
      if (typeof ui[m] !== "function") return false;
      const d = Object.getOwnPropertyDescriptor(ui, m);
      if (d && !d.writable && !d.set) return false;
    }
    return !Object.isFrozen(ui);
  }

  function ensureWrapped(ui: UiContext | undefined, ctx?: HostContext): boolean {
    if (!wrapperOk || !ui || typeof ui !== "object" || headlessOwner) return false;
    if (wrapped.has(ui)) return true;
    if ((ctx ?? lastCtx)?.mode !== "tui") return false;
    if (!shapeOk(ui)) {
      wrapperOk = false; // fall back to screen detection
      return false;
    }
    for (const m of DIALOG_METHODS) {
      const current = ui[m] as unknown as ((...a: unknown[]) => Promise<unknown>) & { [WRAPPED_MARK]?: boolean };
      if (current[WRAPPED_MARK]) continue; // another copy of us already wrapped this method
      const orig = current.bind(ui);
      const w = (...args: unknown[]) => wrappedDialog(m, orig, args);
      (w as any)[WRAPPED_MARK] = true;
      try {
        (ui as any)[m] = w;
        if ((ui as any)[m] !== w) throw new Error("setter ignored");
      } catch {
        // restore what we can and disable
        try {
          (ui as any)[m] = current;
        } catch {
          /* ignore */
        }
        wrapperOk = false;
        return false;
      }
    }
    wrapped.add(ui);
    return true;
  }

  function wrappedDialog(
    m: DialogMethod,
    orig: (...a: unknown[]) => Promise<unknown>,
    args: unknown[],
  ): Promise<unknown> {
    const rawOpts = args[2];
    const callerOpts = rawOpts && typeof rawOpts === "object" ? (rawOpts as Record<string, unknown>) : {};
    const ac = new AbortController();
    const callerSignal = callerOpts.signal as AbortSignal | undefined;
    if (callerSignal) {
      if (callerSignal.aborted) ac.abort();
      else callerSignal.addEventListener("abort", () => ac.abort(), { once: true });
    }
    const nativeArgs = [args[0], args[1], { ...callerOpts, signal: ac.signal }];
    const native = orig(...nativeArgs);

    let gate: ReturnType<VibekeClient["gate"]> | undefined;
    const dialogId = randomUUID();
    try {
      const title = String(args[0] ?? "");
      const payload: Record<string, unknown> = { method: m, title, dialog_id: dialogId };
      if (m === "confirm") payload.message = preview(String(args[1] ?? ""), 2000);
      if (m === "select" && Array.isArray(args[1])) payload.options = args[1].map(String);
      if (m === "input" && typeof args[1] === "string") payload.message = preview(args[1], 2000);
      gate = client.gate("Dialog", payload);
    } catch {
      return native;
    }
    const g = gate;
    const options = m === "select" && Array.isArray(args[1]) ? args[1].map(String) : undefined;
    const valid = (v: unknown) =>
      m === "confirm" ? typeof v === "boolean" : m === "select" ? typeof v === "string" && !!options?.includes(v) : typeof v === "string";

    const vibekeFirst = g.answer.then((a) => {
      if (a && valid(a.value)) return a;
      return new Promise<never>(() => {}); // no usable Vibeke answer: keep waiting for native
    });
    const nativeFirst = native.then(
      (v) => ({ by: "native" as const, v }),
      (e) => ({ by: "native-error" as const, e }),
    );
    return Promise.race([nativeFirst, vibekeFirst.then((a) => ({ by: "vibeke" as const, v: a.value }))]).then((w) => {
      if (w.by === "vibeke") {
        ac.abort(); // dismiss pi's native dialog exactly once
        native.catch(() => {});
        g.close();
        return w.v;
      }
      g.close();
      if (w.by === "native-error") throw (w as { e: unknown }).e;
      emit("DialogResolved", { dialog_id: dialogId, by: "native", value: w.v });
      return w.v;
    });
  }

  // ---- helpers ----
  function sweep(): void {
    const now = Date.now();
    for (const [k, c] of calls) if (now - c.at > CACHE_TTL_MS) calls.delete(k);
  }

  function identify(event: any, ctx: HostContext, fallbackSource: string): void {
    lastCtx = ctx;
    if (ctx.model?.id) model = ctx.model.id;
    ensureWrapped(ctx.ui, ctx);
    emit("SessionStart", {
      session_id: sessionId(),
      transcript_path: sessionFile(),
      source: event?.reason ?? fallbackSource,
      model: model,
      host,
      host_version: hostVersion(),
      extension_version: EXTENSION_VERSION,
    });
  }

  function cancelEnd(): void {
    if (endTimer) clearTimeout(endTimer);
    endTimer = null;
  }

  function endTurn(): void {
    cancelEnd();
    streaming = false;
    if (!turnOpen) return;
    turnOpen = false;
    promptSent = false;
    const e = lastEnd;
    lastEnd = {};
    emit("TurnEnded", { ...(e.stop_reason ? { stop_reason: e.stop_reason } : {}), ...(e.last_message ? { last_message: e.last_message } : {}) });
  }

  function lastAssistant(event: any): { stop_reason?: string; last_message?: string } {
    const msgs = Array.isArray(event?.messages) ? event.messages : [];
    for (let i = msgs.length - 1; i >= 0; i--) {
      const m = msgs[i];
      if (m?.role !== "assistant") continue;
      let text: string | undefined;
      if (typeof m.content === "string") text = m.content;
      else if (Array.isArray(m.content))
        text = m.content
          .filter((c: any) => c?.type === "text" && typeof c.text === "string")
          .map((c: any) => c.text)
          .join("");
      return { stop_reason: m.stopReason, last_message: preview(text, 2000) };
    }
    return {};
  }

  // ---- event handlers ----
  const on = (name: string, fn: (event: any, ctx: HostContext) => void | Promise<void>): void => {
    try {
      pi.on(name, (event: any, ctx: HostContext) => {
        try {
          if (ctx && typeof ctx === "object") {
            lastCtx = ctx;
          }
          return fn(event, ctx);
        } catch {
          return undefined;
        }
      });
    } catch {
      /* host may validate event names */
    }
  };

  for (const n of ["session_start", "session_switch", "session_branch"]) {
    on(n, (e, ctx) => {
      identify(e, ctx, n === "session_start" ? "startup" : n.replace("session_", ""));
    });
  }

  on("input", (e) => {
    if (e?.source === "extension") return;
    cancelEnd();
    if (endTimer) endTurn();
    turnOpen = true;
    promptSent = true;
    emit("TurnStarted", { prompt_preview: preview(e?.text ?? e?.prompt, 200) });
  });

  on("agent_start", () => {
    cancelEnd();
    streaming = true;
    if (!turnOpen) {
      turnOpen = true;
      emit("TurnStarted", {});
    } else if (!promptSent) {
      // already open from a previous agent_start in the same turn: nothing new
    }
    promptSent = false;
    emit("Working", {});
  });

  on("turn_start", () => {
    turnIndex++;
  });

  on("turn_end", (e) => {
    const u = e?.message?.usage;
    if (!u || typeof u !== "object") return;
    emit("Usage", {
      input: u.input ?? 0,
      output: u.output ?? 0,
      cache_read: u.cacheRead ?? 0,
      cache_write: u.cacheWrite ?? 0,
      ...(u.cost?.total !== undefined ? { cost: u.cost.total } : {}),
    });
  });

  // Must stay synchronous and always return undefined: observe-only.
  on("tool_call", (e) => {
    const id = e?.toolCallId;
    if (typeof id !== "string") return;
    sweep();
    const prev = calls.get(id);
    calls.set(id, { tool: e.toolName ?? prev?.tool ?? "", input: e.input, at: Date.now(), started: prev?.started ?? false });
    return undefined;
  });

  on("tool_execution_start", (e) => {
    const id = e?.toolCallId;
    if (typeof id !== "string") return;
    const prev = calls.get(id);
    const tool = e.toolName ?? prev?.tool ?? "";
    const input = prev ? prev.input : e.args;
    calls.set(id, { tool, input, at: Date.now(), started: true });
    emit("ToolStarted", { call_id: id, tool, input: redactInput(input) });
  });

  on("tool_execution_end", (e, ctx) => {
    const id = e?.toolCallId;
    if (typeof id !== "string") return;
    const cached = calls.get(id);
    calls.delete(id);
    const tool = e.toolName ?? cached?.tool ?? "";
    const ok = !e.isError;
    const fp = ok && cached ? fileChangePath(tool, cached.input, ctx?.cwd ?? lastCtx?.cwd) : undefined;
    emit("ToolEnded", { call_id: id, tool, ok, ...(fp ? { file_path: fp } : {}) });
  });

  on("tool_approval_requested", (e) => {
    host = "omp";
    const id = e?.toolCallId;
    if (typeof id !== "string") return;
    approvals.set(id, { tool: e.toolName ?? "", reason: e.reason });
    emit("ApprovalRequested", { call_id: id, tool: e.toolName ?? "", reason: e.reason, approval_mode: e.approvalMode });
  });

  on("tool_approval_resolved", (e) => {
    const id = e?.toolCallId;
    if (typeof id !== "string") return;
    approvals.delete(id);
    emit("ApprovalResolved", { call_id: id, approved: !!e.approved });
  });

  on("agent_end", (e) => {
    lastEnd = lastAssistant(e);
    cancelEnd();
    endTimer = setTimeout(() => {
      endTimer = null;
      endTurn();
    }, debounceMs);
    endTimer.unref?.();
  });

  on("agent_settled", () => {
    endTurn();
  });

  on("session_stop", () => {
    emit("Settling", {});
  });

  on("auto_retry_start", (e) => {
    const message = String(e?.errorMessage ?? e?.message ?? "");
    emit("Error", { message: message.slice(0, 500), retrying: true, rate_limited: RATE_LIMIT_RE.test(message) });
  });

  on("auto_retry_end", () => {
    emit("Working", {});
  });

  const compactStart = () => {
    compacting = true;
    emit("Compacting", { phase: "start" });
  };
  const compactEnd = () => {
    if (!compacting) return;
    compacting = false;
    emit("Compacting", { phase: "end" });
  };
  on("compaction_start", compactStart);
  on("auto_compaction_start", compactStart);
  on("compaction_end", compactEnd);
  on("auto_compaction_end", compactEnd);
  on("session_compact", compactEnd);

  on("model_select", (e) => {
    const id = e?.model?.id ?? e?.modelId;
    if (typeof id === "string") model = id;
  });

  on("session_shutdown", async (e) => {
    cancelEnd();
    endTurn();
    emit("SessionEnded", { reason: e?.reason ?? "shutdown" });
    await client.flush(300);
    client.close();
  });

  // Connect eagerly so the first Snapshot is sent at load.
  try {
    client.start();
  } catch {
    /* ignore */
  }

  return {
    client,
    get host() {
      return host;
    },
    ensureWrapped,
    wrapperEnabled: () => wrapperOk,
  } as Handle;
}

function safe<T>(fn: () => T): T | undefined {
  try {
    return fn();
  } catch {
    return undefined;
  }
}

export default function vibekeExtension(pi: HostApi): void {
  try {
    createExtension(pi);
  } catch {
    /* never break the host */
  }
}
