// src/index.ts
import { execFile } from "node:child_process";
import { randomUUID } from "node:crypto";

// src/describe.ts
import { isAbsolute, resolve } from "node:path";
var MAX_INPUT_BYTES = 8 * 1024;
var SECRET_KEY = /(key|token|secret|password|authorization)/i;
var MAX_DEPTH = 8;
function redactValue(v, depth, seen) {
  if (v === null || typeof v !== "object") {
    return typeof v === "bigint" || typeof v === "function" || typeof v === "symbol" ? String(v) : v;
  }
  if (seen.has(v))
    return "[circular]";
  if (depth >= MAX_DEPTH)
    return "[depth]";
  seen.add(v);
  let out;
  if (Array.isArray(v)) {
    out = v.map((x) => redactValue(x, depth + 1, seen));
  } else {
    const o = {};
    for (const [k, val] of Object.entries(v)) {
      o[k] = SECRET_KEY.test(k) ? "[redacted]" : redactValue(val, depth + 1, seen);
    }
    out = o;
  }
  seen.delete(v);
  return out;
}
function redactInput(input) {
  try {
    const r = redactValue(input, 0, new WeakSet);
    const json = JSON.stringify(r);
    if (json !== undefined && json.length > MAX_INPUT_BYTES) {
      return { _truncated: true, _bytes: json.length, preview: json.slice(0, MAX_INPUT_BYTES) };
    }
    return r;
  } catch {
    return { _unserializable: true };
  }
}
var MAX_PROMPT_BYTES = 8 * 1024;
function boundedPrompt(s, maxBytes = MAX_PROMPT_BYTES) {
  if (typeof s !== "string" || s.length === 0)
    return { truncated: false };
  const bytes = new TextEncoder().encode(s);
  if (bytes.length <= maxBytes)
    return { prompt: s, truncated: false };
  let end = maxBytes;
  while (end > 0 && (bytes[end] & 192) === 128)
    end--;
  return { prompt: new TextDecoder().decode(bytes.subarray(0, end)), truncated: true };
}
function preview(s, n) {
  return typeof s === "string" && s.length > 0 ? s.slice(0, n) : undefined;
}
var FILE_TOOL = /(^|[_-])(write|edit|multiedit|patch)/i;
function fileChangePath(tool, input, cwd) {
  if (!FILE_TOOL.test(tool) || !input || typeof input !== "object")
    return;
  const o = input;
  const p = [o.path, o.file_path, o.filePath, o.file].find((x) => typeof x === "string" && x.length > 0);
  if (!p)
    return;
  return isAbsolute(p) ? p : resolve(cwd ?? process.cwd(), p);
}
var RATE_LIMIT_RE = /overloaded|rate.?limit|429|5\d\d|timeout/i;

// src/protocol.ts
import * as net from "node:net";
import { StringDecoder } from "node:string_decoder";
var MAX_PENDING_ACKS = 100;
function lineReader(onLine) {
  let buf = "";
  const dec = new StringDecoder("utf8");
  return (chunk) => {
    buf += typeof chunk === "string" ? chunk : dec.write(chunk);
    let i;
    while ((i = buf.indexOf(`
`)) >= 0) {
      const line = buf.slice(0, i);
      buf = buf.slice(i + 1);
      if (line.trim())
        onLine(line);
    }
  };
}

class VibekeClient {
  o;
  sock = null;
  ready = false;
  blocked = false;
  connecting = false;
  closed = false;
  everReady = false;
  queue = [];
  lastSeq = 0;
  rpcId = 0;
  tries = 0;
  lastBurstEnd = 0;
  timer = null;
  acks = new Map;
  dropped = 0;
  connects = 0;
  gateReconnects = 0;
  controlHandled = 0;
  controlStarted = false;
  takes = new Map;
  controlSock = null;
  cap;
  minMs;
  maxMs;
  maxTries;
  constructor(o) {
    this.o = o;
    this.cap = o.queueCap ?? 500;
    this.minMs = o.backoffMinMs ?? 50;
    this.maxMs = o.backoffMaxMs ?? 2000;
    this.maxTries = o.maxTries ?? 5;
  }
  nextSeq() {
    const base = Date.now() * 1000;
    this.lastSeq = base > this.lastSeq ? base : this.lastSeq + 1;
    return this.lastSeq;
  }
  get harness() {
    return typeof this.o.harness === "function" ? this.o.harness() : this.o.harness;
  }
  start() {
    this.ensureConnecting();
  }
  frame(method, params) {
    return JSON.stringify({ jsonrpc: "2.0", id: ++this.rpcId, method, params }) + `
`;
  }
  signal(event, payload = {}) {
    const seq = this.nextSeq();
    if (this.closed)
      return seq;
    try {
      const line = this.frame("adapter.signal", {
        harness: this.harness,
        pid: process.pid,
        event,
        payload: { ...payload, seq }
      });
      if (this.queue.length >= this.cap) {
        this.queue.length = 0;
        this.dropped++;
      }
      this.queue.push(line);
      if (this.ready)
        this.pump();
      else
        this.ensureConnecting();
    } catch {}
    return seq;
  }
  ensureConnecting() {
    if (this.closed || this.connecting || this.ready || this.timer)
      return;
    if (this.tries >= this.maxTries) {
      if (Date.now() - this.lastBurstEnd < this.maxMs)
        return;
      this.tries = 0;
    }
    this.connect();
  }
  connect() {
    this.connecting = true;
    let s;
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
    s.on("end", () => s.destroy());
    s.on("data", lineReader((line) => {
      try {
        const m = JSON.parse(line);
        if (typeof m?.id === "number") {
          this.acks.delete(m.id);
          const done = this.takes.get(m.id);
          if (done) {
            const c = m.result?.context;
            done(!m.error && typeof c === "string" && c ? c : null);
          }
        }
      } catch {}
    }));
    s.on("connect", () => {
      this.connecting = false;
      this.tries = 0;
      this.connects++;
      try {
        s.write(this.frame("client.hello", {
          client: "vibeke-pi-extension",
          kind: "agent",
          token: this.o.token,
          version: this.o.version
        }));
        const trusted = this.connects === 1 && this.dropped === 0 && !this.everReady && this.queue.length > 0;
        let snap = {};
        try {
          snap = this.o.snapshot(trusted);
        } catch {}
        let seq;
        if (trusted) {
          const m = /"seq":(\d+)/.exec(this.queue[0]);
          seq = m ? Number(m[1]) - 1 : this.nextSeq();
        } else {
          this.queue.length = 0;
          seq = this.nextSeq();
        }
        s.write(this.frame("adapter.signal", {
          harness: this.harness,
          pid: process.pid,
          event: "Snapshot",
          payload: { ...snap, seq }
        }));
        for (const line of this.acks.values())
          s.write(line);
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
      if (this.sock === s)
        this.sock = null;
      for (const done of [...this.takes.values()])
        done(null);
      const wasReady = this.ready;
      this.ready = false;
      this.blocked = false;
      this.connecting = false;
      if (this.closed)
        return;
      if (wasReady)
        this.tries = 0;
      this.scheduleRetry();
    });
  }
  scheduleRetry() {
    if (this.closed || this.timer)
      return;
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
  pump() {
    const s = this.sock;
    if (!s || !this.ready || this.blocked)
      return;
    try {
      while (this.queue.length) {
        const line = this.queue.shift();
        if (!s.write(line)) {
          this.blocked = true;
          return;
        }
      }
    } catch {}
  }
  deliveryAck(interaction, idempotencyKey) {
    if (this.closed)
      return;
    try {
      const id = ++this.rpcId;
      const line = JSON.stringify({
        jsonrpc: "2.0",
        id,
        method: "adapter.delivery_ack",
        params: { interaction, idempotency_key: idempotencyKey, applied: true }
      }) + `
`;
      if (this.acks.size >= MAX_PENDING_ACKS) {
        const oldest = this.acks.keys().next().value;
        if (oldest !== undefined)
          this.acks.delete(oldest);
      }
      this.acks.set(id, line);
      if (this.ready && this.sock)
        this.sock.write(line);
      else
        this.ensureConnecting();
    } catch {}
  }
  take(timeoutMs = 2000) {
    return new Promise((resolve2) => {
      try {
        const s = this.sock;
        if (this.closed || !this.ready || !s)
          return resolve2(null);
        const id = ++this.rpcId;
        const line = JSON.stringify({ jsonrpc: "2.0", id, method: "message.take", params: {} }) + `
`;
        let timer = null;
        const done = (v) => {
          if (timer)
            clearTimeout(timer);
          this.takes.delete(id);
          resolve2(v);
        };
        timer = setTimeout(() => done(null), timeoutMs);
        timer.unref?.();
        this.takes.set(id, done);
        s.write(line);
      } catch {
        resolve2(null);
      }
    });
  }
  get pendingAcks() {
    return this.acks.size;
  }
  flush(timeoutMs) {
    const deadline = Date.now() + timeoutMs;
    return new Promise((resolve2) => {
      const tick = () => {
        const drained = this.ready && this.queue.length === 0 && (this.sock?.writableLength ?? 0) === 0;
        if (drained || Date.now() >= deadline || this.closed)
          return resolve2();
        setTimeout(tick, 5);
      };
      tick();
    });
  }
  get connected() {
    return this.ready;
  }
  close() {
    this.closed = true;
    if (this.timer)
      clearTimeout(this.timer);
    this.timer = null;
    this.queue.length = 0;
    try {
      this.sock?.destroy();
      this.controlSock?.destroy();
    } catch {}
  }
  control(ops, handle) {
    if (this.controlStarted || this.closed || ops.length === 0)
      return;
    this.controlStarted = true;
    let tries = 0;
    let reply;
    const retry = () => {
      if (this.closed)
        return;
      const delay = Math.min(this.minMs * 2 ** Math.min(tries, 16), this.maxMs);
      tries++;
      const t = setTimeout(connect, delay);
      t.unref?.();
    };
    const connect = () => {
      if (this.closed)
        return;
      let sock;
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
        if (this.closed || sock.destroyed)
          return;
        id++;
        sentAt = Date.now();
        const params = { harness: this.harness, ops };
        if (reply)
          params.reply = reply;
        reply = undefined;
        try {
          sock.write(JSON.stringify({ jsonrpc: "2.0", id, method: "adapter.control", params }) + `
`);
        } catch {
          sock.destroy();
        }
      };
      sock.on("data", lineReader((line) => {
        let m;
        try {
          m = JSON.parse(line);
        } catch {
          return;
        }
        if (m?.id !== id)
          return;
        const r = m.result;
        if (m.error || !r || typeof r !== "object" || !("request" in r)) {
          stopped = true;
          sock.destroy();
          return;
        }
        tries = 0;
        const req = r.request;
        if (req && typeof req === "object" && typeof req.id === "string") {
          const params = req.params && typeof req.params === "object" ? req.params : {};
          Promise.resolve().then(() => handle(String(req.op), params)).then((result) => ({ id: req.id, ok: true, result: result ?? null }), (e) => ({ id: req.id, ok: false, error: String(e?.message ?? e).slice(0, 500) })).then((rep) => {
            this.controlHandled++;
            reply = rep;
            poll();
          });
        } else if (Date.now() - sentAt < 1000) {
          const t = setTimeout(poll, 1000);
          t.unref?.();
        } else {
          poll();
        }
      }));
      sock.on("connect", () => {
        try {
          sock.write(JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "client.hello",
            params: { client: "vibeke-pi-extension", kind: "agent", token: this.o.token, version: this.o.version }
          }) + `
`);
        } catch {
          sock.destroy();
          return;
        }
        poll();
      });
      sock.on("close", () => {
        if (this.controlSock === sock)
          this.controlSock = null;
        if (!stopped)
          retry();
      });
    };
    connect();
  }
  gate(event, payload) {
    let settle;
    let done = false;
    const answer = new Promise((r) => {
      settle = (v) => {
        if (!done) {
          done = true;
          r(v);
        }
      };
    });
    let s = null;
    let timer = null;
    let tries = 0;
    let attempts = 0;
    const gateId = 2;
    const stop = () => {
      if (timer)
        clearTimeout(timer);
      timer = null;
      try {
        s?.destroy();
      } catch {}
      s = null;
    };
    const retry = () => {
      if (done || timer)
        return;
      if (this.closed)
        return settle(null);
      const delay = Math.min(this.minMs * 2 ** tries, this.maxMs);
      tries++;
      timer = setTimeout(() => {
        timer = null;
        attempt();
      }, delay);
      timer.unref?.();
    };
    const attempt = () => {
      if (done)
        return;
      if (this.closed)
        return settle(null);
      if (attempts++ > 0)
        this.gateReconnects++;
      let sock;
      try {
        sock = net.createConnection(this.o.socketPath);
      } catch {
        retry();
        return;
      }
      s = sock;
      sock.unref();
      sock.on("error", () => {});
      sock.on("end", () => sock.destroy());
      sock.on("close", () => {
        if (s === sock)
          s = null;
        retry();
      });
      sock.on("data", lineReader((line) => {
        try {
          const m = JSON.parse(line);
          if (m.id !== gateId)
            return;
          const r = m.result;
          const d = r?.decision;
          if (d && typeof d === "object" && "value" in d) {
            settle({
              value: d.value,
              interaction: typeof r.interaction === "string" ? r.interaction : undefined,
              idempotencyKey: typeof r.idempotency_key === "string" ? r.idempotency_key : undefined
            });
          } else {
            settle(null);
          }
        } catch {
          settle(null);
        }
        stop();
      }));
      sock.on("connect", () => {
        try {
          sock.write(JSON.stringify({
            jsonrpc: "2.0",
            id: 1,
            method: "client.hello",
            params: { client: "vibeke-pi-extension", kind: "agent", token: this.o.token, version: this.o.version }
          }) + `
`);
          sock.write(JSON.stringify({
            jsonrpc: "2.0",
            id: gateId,
            method: "adapter.gate",
            params: { harness: this.harness, pid: process.pid, event, payload }
          }) + `
`);
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
      }
    };
  }
}

// src/version.ts
var EXTENSION_VERSION = "0.1.0";

// src/index.ts
var CACHE_TTL_MS = 10 * 60 * 1000;
var WRAPPED_MARK = Symbol.for("vibeke.wrapped");
var DIALOG_METHODS = ["confirm", "select", "input"];
function detectHost(env) {
  const forced = env.VIBEKE_PI_HOST;
  if (forced === "pi" || forced === "omp")
    return forced;
  return process.versions.bun ? "omp" : "pi";
}
function createExtension(pi, opts = {}) {
  const env = opts.env ?? process.env;
  if (env.VIBEKE !== "1" || !env.VIBEKE_SOCKET || !env.VIBEKE_PANE_TOKEN)
    return;
  let host = opts.host ?? detectHost(env);
  const headlessOwner = env.VIBEKE_HEADLESS_OWNER === "1";
  const debounceMs = opts.debounceMs ?? 250;
  let lastCtx;
  let model;
  let currentModel;
  let turnIndex = 0;
  let streaming = false;
  let turnOpen = false;
  let promptSent = false;
  let compacting = false;
  let endTimer = null;
  let lastEnd = {};
  const calls = new Map;
  const approvals = new Map;
  const dialogs = new Map;
  const sessionId = () => safe(() => lastCtx?.sessionManager?.getSessionId?.()) ?? null;
  const sessionFile = () => safe(() => lastCtx?.sessionManager?.getSessionFile?.()) ?? null;
  const client = new VibekeClient({
    socketPath: env.VIBEKE_SOCKET,
    token: env.VIBEKE_PANE_TOKEN,
    harness: () => host,
    version: EXTENSION_VERSION,
    snapshot: (queueKept) => ({
      session_id: sessionId(),
      session_file: sessionFile(),
      is_streaming: streaming,
      turn_index: turnIndex,
      model: model ?? null,
      host,
      host_version: hostVersion(),
      extension_version: EXTENSION_VERSION,
      pending_tool_calls: [...calls.entries()].filter(([, c]) => c.started).map(([call_id, c]) => ({ call_id, tool: c.tool, input: redactInput(c.input) })),
      open_approvals: [...approvals.entries()].map(([call_id, a]) => ({
        call_id,
        tool: a.tool,
        reason: a.reason
      })),
      pending_dialogs: [...dialogs.values()],
      ...bgAfterConnect(queueKept)
    }),
    ...opts.clientOverrides
  });
  const hostVersion = () => typeof pi.version === "string" ? pi.version : undefined;
  const HEADLESS_ALLOWED = new Set(["SessionStart", "ToolEnded", "Snapshot", "BgTasks"]);
  function emit(event, payload = {}) {
    if (headlessOwner && !HEADLESS_ALLOWED.has(event))
      return;
    try {
      if (lastCtx?.ui)
        ensureWrapped(lastCtx.ui, lastCtx);
      client.signal(event, payload);
    } catch {}
  }
  const BG_EMPTY = "[]";
  const BG_POLL_MS = opts.bgPollMs ?? 2000;
  let lastBg = BG_EMPTY;
  let bgSeen = false;
  let bgTimer = null;
  let shutDown = false;
  const jobCalls = new Map;
  const piRuns = new Map;
  const ompJobsApi = () => typeof lastCtx?.getAsyncJobSnapshot === "function";
  function ompJobs() {
    if (!ompJobsApi())
      return [];
    const snap = safe(() => lastCtx.getAsyncJobSnapshot());
    const running = snap && Array.isArray(snap.running) ? snap.running : [];
    const out = [];
    for (const j of running) {
      if (!j || typeof j.id !== "string" || !j.id || j.status !== undefined && j.status !== "running")
        continue;
      const call = jobCalls.get(j.id);
      out.push({
        id: j.id,
        type: j.type === "task" ? "subagent" : j.type === "bash" ? "shell" : String(j.type ?? "task"),
        status: "running",
        description: typeof j.label === "string" ? j.label : "",
        ...call ? { tool_use_id: call } : {}
      });
    }
    return out;
  }
  function alive(pid) {
    if (pid === undefined)
      return true;
    try {
      process.kill(pid, 0);
      return true;
    } catch (e) {
      return e?.code === "EPERM";
    }
  }
  function piRunTasks() {
    const sid = sessionId();
    const out = [];
    for (const [id, r] of piRuns) {
      if (!alive(r.pid)) {
        piRuns.delete(id);
        continue;
      }
      if (r.sessionId && sid && r.sessionId !== sid)
        continue;
      out.push({ id, type: "subagent", status: "running", description: r.description });
    }
    return out;
  }
  function checkBg(force = false) {
    if (shutDown)
      return;
    let list;
    try {
      list = [...ompJobs(), ...piRunTasks()];
    } catch {
      return;
    }
    const key = JSON.stringify(list);
    if (key !== lastBg || force && (bgSeen || ompJobsApi())) {
      lastBg = key;
      if (list.length)
        bgSeen = true;
      const kinds = ompJobsApi() ? ["shell", "subagent"] : pi.events ? ["subagent"] : [];
      emit("BgTasks", { background_tasks: list, kinds });
    }
    if (bgTimer)
      clearTimeout(bgTimer);
    bgTimer = null;
    if (list.length) {
      bgTimer = setTimeout(() => {
        bgTimer = null;
        checkBg();
      }, BG_POLL_MS);
      bgTimer.unref?.();
    }
  }
  function bgAfterConnect(queueKept) {
    if (!queueKept && bgSeen && lastBg !== null) {
      lastBg = null;
      const t = setTimeout(() => checkBg(), 0);
      t.unref?.();
    }
    return {};
  }
  function rememberJobCalls(callId, result) {
    const d = result?.details;
    const ids = [];
    if (typeof d?.async?.jobId === "string")
      ids.push(d.async.jobId);
    if (d?.async && Array.isArray(d.progress)) {
      for (const p of d.progress)
        if (typeof p?.id === "string" && (p.status === "pending" || p.status === "running"))
          ids.push(p.id);
    }
    if (!ids.length)
      return;
    if (jobCalls.size > 500)
      jobCalls.clear();
    for (const id of ids)
      jobCalls.set(id, callId);
  }
  function subscribeSubagents() {
    const bus = pi.events;
    if (!bus || typeof bus.on !== "function")
      return;
    try {
      bus.on("subagent:async-started", (data) => {
        try {
          if (!data || typeof data.id !== "string" || !data.id)
            return;
          const agents = Array.isArray(data.agents) ? data.agents.filter((a) => typeof a === "string") : [];
          const who = typeof data.agent === "string" ? data.agent : agents.join(", ");
          const what = typeof data.goal === "string" && data.goal ? data.goal : typeof data.task === "string" ? data.task : "";
          piRuns.set(data.id, {
            description: [who, what].filter(Boolean).join(": "),
            ...typeof data.pid === "number" ? { pid: data.pid } : {},
            ...typeof data.sessionId === "string" ? { sessionId: data.sessionId } : {}
          });
          checkBg();
        } catch {}
      });
      bus.on("subagent:async-complete", (data) => {
        try {
          const id = typeof data?.runId === "string" ? data.runId : data?.id;
          if (typeof id === "string" && piRuns.delete(id))
            checkBg();
        } catch {}
      });
    } catch {}
  }
  const wrapped = new WeakSet;
  let wrapperOk = true;
  function shapeOk(ui) {
    for (const m of DIALOG_METHODS) {
      if (typeof ui[m] !== "function")
        return false;
      const d = Object.getOwnPropertyDescriptor(ui, m);
      if (d && !d.writable && !d.set)
        return false;
    }
    return !Object.isFrozen(ui);
  }
  function ensureWrapped(ui, ctx) {
    if (!wrapperOk || !ui || typeof ui !== "object" || headlessOwner)
      return false;
    if (wrapped.has(ui))
      return true;
    if ((ctx ?? lastCtx)?.mode !== "tui")
      return false;
    if (!shapeOk(ui)) {
      wrapperOk = false;
      return false;
    }
    for (const m of DIALOG_METHODS) {
      const current = ui[m];
      if (current[WRAPPED_MARK])
        continue;
      const orig = current.bind(ui);
      const w = (...args) => wrappedDialog(m, orig, args);
      w[WRAPPED_MARK] = true;
      try {
        ui[m] = w;
        if (ui[m] !== w)
          throw new Error("setter ignored");
      } catch {
        try {
          ui[m] = current;
        } catch {}
        wrapperOk = false;
        return false;
      }
    }
    wrapped.add(ui);
    return true;
  }
  function wrappedDialog(m, orig, args) {
    const rawOpts = args[2];
    const callerOpts = rawOpts && typeof rawOpts === "object" ? rawOpts : {};
    const ac = new AbortController;
    const callerSignal = callerOpts.signal;
    if (callerSignal) {
      if (callerSignal.aborted)
        ac.abort();
      else
        callerSignal.addEventListener("abort", () => ac.abort(), { once: true });
    }
    const nativeArgs = [args[0], args[1], { ...callerOpts, signal: ac.signal }];
    const native = orig(...nativeArgs);
    let gate;
    const dialogId = randomUUID();
    try {
      const title = String(args[0] ?? "");
      const payload = { method: m, title, dialog_id: dialogId };
      if (m === "confirm")
        payload.message = preview(String(args[1] ?? ""), 2000);
      if (m === "select" && Array.isArray(args[1]))
        payload.options = args[1].map(String);
      if (m === "input" && typeof args[1] === "string")
        payload.message = preview(args[1], 2000);
      dialogs.set(dialogId, payload);
      gate = client.gate("Dialog", payload);
    } catch {
      dialogs.delete(dialogId);
      return native;
    }
    const g = gate;
    const options = m === "select" && Array.isArray(args[1]) ? args[1].map(String) : undefined;
    const valid = (v) => m === "confirm" ? typeof v === "boolean" : m === "select" ? typeof v === "string" && !!options?.includes(v) : typeof v === "string";
    const vibekeFirst = g.answer.then((a) => {
      if (a && valid(a.value))
        return a;
      return new Promise(() => {});
    });
    const nativeFirst = native.then((v) => ({ by: "native", v }), (e) => ({ by: "native-error", e }));
    return Promise.race([nativeFirst, vibekeFirst.then((a) => ({ by: "vibeke", v: a.value, a }))]).then((w) => {
      dialogs.delete(dialogId);
      if (w.by === "vibeke") {
        ac.abort();
        native.catch(() => {});
        g.close();
        const { interaction, idempotencyKey } = w.a;
        if (interaction && idempotencyKey) {
          try {
            client.deliveryAck(interaction, idempotencyKey);
          } catch {}
        }
        return w.v;
      }
      g.close();
      if (w.by === "native-error")
        throw w.e;
      emit("DialogResolved", { dialog_id: dialogId, by: "native", value: w.v });
      return w.v;
    });
  }
  const RIDE_TYPE = "vibeke-message";
  let agentRunning = false;
  let taking = null;
  function take() {
    if (headlessOwner)
      return Promise.resolve(null);
    if (taking)
      return Promise.resolve(null);
    const p = client.take().catch(() => null);
    taking = p;
    const clear = () => {
      if (taking === p)
        taking = null;
    };
    p.then(clear, clear);
    return p;
  }
  function rideMessage(content) {
    return { customType: RIDE_TYPE, content, display: true };
  }
  function ride(content, options) {
    try {
      if (typeof pi.sendMessage === "function")
        pi.sendMessage(rideMessage(content), options);
    } catch {}
  }
  function sweep() {
    const now = Date.now();
    for (const [k, c] of calls)
      if (now - c.at > CACHE_TTL_MS)
        calls.delete(k);
  }
  async function available() {
    const reg = lastCtx?.modelRegistry;
    if (!reg || typeof reg.getAvailable !== "function")
      throw new Error("the host exposes no model registry");
    const list = await reg.getAvailable();
    return Array.isArray(list) ? list : [];
  }
  async function onControl(op, params) {
    if (op === "models") {
      const cur = modelKey(currentModel ?? lastCtx?.model ?? undefined);
      const models = (await available()).slice(0, 500).map((m) => {
        const id = modelKey(m);
        if (!id)
          return;
        return {
          id,
          label: typeof m.name === "string" && m.name ? m.name : String(m.id),
          ...typeof m.provider === "string" ? { description: m.provider } : {},
          current: id === cur
        };
      }).filter((m) => m !== undefined);
      return { models };
    }
    if (op === "set_model") {
      const want = String(params.model ?? "");
      if (params.scope === "default" && host === "omp")
        throw new Error("oh-my-pi switches models for the session only");
      if (params.scope !== "default" && host === "pi")
        throw new Error("persists_default: pi saves every model switch as its default model");
      if (typeof pi.setModel !== "function")
        throw new Error("the host cannot switch models");
      const list = await available();
      const slash = want.indexOf("/");
      const m = list.find((x) => modelKey(x) === want) ?? (slash > 0 ? lastCtx?.modelRegistry?.find?.(want.slice(0, slash), want.slice(slash + 1)) : undefined) ?? list.find((x) => x.id === want);
      if (!m)
        throw new Error(`unknown model: ${want}`);
      const ok = await pi.setModel(m);
      if (ok === false)
        throw new Error(`no credentials for ${m.provider ?? "this provider"}`);
      currentModel = m;
      if (typeof m.id === "string")
        model = m.id;
      return { model: modelKey(m), default_changed: host === "pi" };
    }
    if (op === "commands") {
      if (typeof pi.getCommands !== "function")
        throw new Error("the host cannot list commands");
      const cmds = pi.getCommands() ?? [];
      return {
        commands: cmds.slice(0, 500).filter((c) => typeof c?.name === "string").map((c) => ({ name: c.name, description: typeof c.description === "string" ? c.description : "" }))
      };
    }
    throw new Error(`unknown request: ${op}`);
  }
  function startControl(ctx) {
    if (headlessOwner || opts.control === false || ctx.mode !== "tui")
      return;
    const ops = [];
    if (ctx.modelRegistry && typeof ctx.modelRegistry.getAvailable === "function") {
      ops.push("models");
      if (typeof pi.setModel === "function")
        ops.push("set_model");
    }
    if (typeof pi.getCommands === "function")
      ops.push("commands");
    client.control(ops, onControl);
  }
  function identify(event, ctx, fallbackSource) {
    lastCtx = ctx;
    if (ctx.model)
      currentModel = ctx.model;
    if (ctx.model?.id)
      model = ctx.model.id;
    ensureWrapped(ctx.ui, ctx);
    emit("SessionStart", {
      session_id: sessionId(),
      transcript_path: sessionFile(),
      source: event?.reason ?? fallbackSource,
      model,
      host,
      host_version: hostVersion(),
      extension_version: EXTENSION_VERSION
    });
    lastBg = BG_EMPTY;
  }
  function cancelEnd() {
    if (endTimer)
      clearTimeout(endTimer);
    endTimer = null;
  }
  function endTurn() {
    cancelEnd();
    streaming = false;
    if (!turnOpen)
      return;
    turnOpen = false;
    promptSent = false;
    const e = lastEnd;
    lastEnd = {};
    emit("TurnEnded", { ...e.stop_reason ? { stop_reason: e.stop_reason } : {}, ...e.last_message ? { last_message: e.last_message } : {} });
    checkBg(true);
  }
  function lastAssistant(event) {
    const msgs = Array.isArray(event?.messages) ? event.messages : [];
    for (let i = msgs.length - 1;i >= 0; i--) {
      const m = msgs[i];
      if (m?.role !== "assistant")
        continue;
      let text;
      if (typeof m.content === "string")
        text = m.content;
      else if (Array.isArray(m.content))
        text = m.content.filter((c) => c?.type === "text" && typeof c.text === "string").map((c) => c.text).join("");
      return { stop_reason: m.stopReason, last_message: preview(text, 2000) };
    }
    return {};
  }
  const on = (name, fn) => {
    try {
      pi.on(name, (event, ctx) => {
        try {
          if (ctx && typeof ctx === "object") {
            lastCtx = ctx;
            startControl(ctx);
          }
          const r = fn(event, ctx);
          checkBg();
          return r;
        } catch {
          return;
        }
      });
    } catch {}
  };
  for (const n of ["session_start", "session_switch", "session_branch"]) {
    on(n, (e, ctx) => {
      identify(e, ctx, n === "session_start" ? "startup" : n.replace("session_", ""));
    });
  }
  on("input", (e) => {
    if (e?.source === "extension")
      return;
    cancelEnd();
    if (endTimer)
      endTurn();
    turnOpen = true;
    promptSent = true;
    const raw = e?.text ?? e?.prompt;
    const { prompt, truncated } = boundedPrompt(raw);
    emit("TurnStarted", {
      prompt_preview: preview(raw, 200),
      ...prompt !== undefined ? { prompt, prompt_truncated: truncated } : {}
    });
  });
  on("before_agent_start", async () => {
    const context = await take();
    return context ? { message: rideMessage(context) } : undefined;
  });
  on("agent_start", () => {
    agentRunning = true;
    cancelEnd();
    streaming = true;
    if (!turnOpen) {
      turnOpen = true;
      emit("TurnStarted", {});
    } else if (!promptSent) {}
    promptSent = false;
    emit("Working", {});
  });
  on("turn_start", () => {
    turnIndex++;
  });
  on("turn_end", (e) => {
    const u = e?.message?.usage;
    if (!u || typeof u !== "object")
      return;
    emit("Usage", {
      input: u.input ?? 0,
      output: u.output ?? 0,
      cache_read: u.cacheRead ?? 0,
      cache_write: u.cacheWrite ?? 0,
      ...u.cost?.total !== undefined ? { cost: u.cost.total } : {}
    });
  });
  function runVibeke(args) {
    if (opts.runVibeke)
      return opts.runVibeke(args);
    const bin = env.VIBEKE_BIN || "vibeke";
    return new Promise((resolve2, reject) => {
      execFile(bin, args, { env, timeout: 60000 }, (err, stdout, stderr) => {
        if (err)
          reject(new Error((stderr || stdout || err.message).toString().trim()));
        else
          resolve2(stdout.toString().trim());
      });
    });
  }
  function registerShowImage() {
    if (typeof pi.registerTool !== "function")
      return;
    try {
      pi.registerTool({
        name: "show_image",
        label: "Show an image to the user",
        description: "Attach an image file (PNG or JPEG), such as a screenshot you took, so the user can see it in Vibeke on any device: the terminal interface, the desktop app or their phone. Use it whenever you produce screenshots the user should look at.",
        parameters: {
          type: "object",
          properties: {
            path: { type: "string", description: "Path to the PNG or JPEG file." },
            caption: { type: "string", description: "Optional short caption shown with the image." }
          },
          required: ["path"]
        },
        async execute(_id, params) {
          const path = typeof params?.path === "string" ? params.path : "";
          if (!path)
            throw new Error("path is required");
          const args = ["screenshot", "add", path];
          if (typeof params.caption === "string" && params.caption)
            args.push("--caption", params.caption);
          args.push("--json");
          const out = await runVibeke(args);
          return { content: [{ type: "text", text: out || "Image attached." }], details: {} };
        }
      });
    } catch {}
  }
  on("tool_call", (e) => {
    const id = e?.toolCallId;
    if (typeof id !== "string")
      return;
    sweep();
    const prev = calls.get(id);
    calls.set(id, { tool: e.toolName ?? prev?.tool ?? "", input: e.input, at: Date.now(), started: prev?.started ?? false });
    return;
  });
  on("tool_execution_start", (e) => {
    const id = e?.toolCallId;
    if (typeof id !== "string")
      return;
    const prev = calls.get(id);
    const tool = e.toolName ?? prev?.tool ?? "";
    const input = prev ? prev.input : e.args;
    calls.set(id, { tool, input, at: Date.now(), started: true });
    emit("ToolStarted", { call_id: id, tool, input: redactInput(input) });
  });
  on("tool_execution_end", (e, ctx) => {
    const id = e?.toolCallId;
    if (typeof id !== "string")
      return;
    const cached = calls.get(id);
    calls.delete(id);
    const tool = e.toolName ?? cached?.tool ?? "";
    const ok = !e.isError;
    if (ok)
      rememberJobCalls(id, e.result);
    const fp = ok && cached ? fileChangePath(tool, cached.input, ctx?.cwd ?? lastCtx?.cwd) : undefined;
    emit("ToolEnded", { call_id: id, tool, ok, ...fp ? { file_path: fp } : {} });
    if (typeof pi.sendMessage === "function") {
      take().then((context) => {
        if (!context)
          return;
        ride(context, agentRunning ? { deliverAs: "steer" } : { deliverAs: "followUp", triggerTurn: true });
      });
    }
  });
  on("tool_approval_requested", (e) => {
    host = "omp";
    const id = e?.toolCallId;
    if (typeof id !== "string")
      return;
    approvals.set(id, { tool: e.toolName ?? "", reason: e.reason });
    emit("ApprovalRequested", { call_id: id, tool: e.toolName ?? "", reason: e.reason, approval_mode: e.approvalMode });
  });
  on("tool_approval_resolved", (e) => {
    const id = e?.toolCallId;
    if (typeof id !== "string")
      return;
    approvals.delete(id);
    emit("ApprovalResolved", { call_id: id, approved: !!e.approved });
  });
  function rideTurnEnd() {
    if (typeof pi.sendMessage !== "function")
      return;
    take().then((context) => {
      if (context)
        ride(context, { deliverAs: "followUp", triggerTurn: true });
    });
  }
  on("agent_end", (e) => {
    agentRunning = false;
    lastEnd = lastAssistant(e);
    cancelEnd();
    endTimer = setTimeout(() => {
      endTimer = null;
      endTurn();
    }, debounceMs);
    endTimer.unref?.();
    rideTurnEnd();
  });
  on("agent_settled", () => {
    endTurn();
    rideTurnEnd();
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
    if (!compacting)
      return;
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
    if (typeof id === "string")
      model = id;
    if (e?.model && typeof e.model === "object")
      currentModel = e.model;
  });
  on("session_shutdown", async (e) => {
    cancelEnd();
    endTurn();
    shutDown = true;
    if (bgTimer)
      clearTimeout(bgTimer);
    bgTimer = null;
    emit("SessionEnded", { reason: e?.reason ?? "shutdown" });
    await client.flush(300);
    client.close();
  });
  registerShowImage();
  subscribeSubagents();
  try {
    client.start();
  } catch {}
  return {
    client,
    get host() {
      return host;
    },
    ensureWrapped,
    wrapperEnabled: () => wrapperOk
  };
}
function modelKey(m) {
  if (!m || typeof m.id !== "string" || !m.id)
    return;
  return typeof m.provider === "string" && m.provider ? `${m.provider}/${m.id}` : m.id;
}
function safe(fn) {
  try {
    return fn();
  } catch {
    return;
  }
}
function vibekeExtension(pi) {
  try {
    createExtension(pi);
  } catch {}
}
export {
  vibekeExtension as default,
  createExtension,
  EXTENSION_VERSION
};
