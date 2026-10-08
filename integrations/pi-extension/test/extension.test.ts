import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { createExtension, EXTENSION_VERSION } from "../src/index.ts";
import { redactInput } from "../src/describe.ts";
import { FakeHost, FakeServer, fakeUi, makeCtx, setup, sleep, waitFor } from "./helpers.ts";

let server: FakeServer;
beforeEach(async () => {
  server = new FakeServer();
  await server.listen();
});
afterEach(async () => {
  await server.kill();
  server.cleanup();
});

describe("activation", () => {
  test("version matches package.json", () => {
    const pkg = JSON.parse(readFileSync(new URL("../package.json", import.meta.url), "utf8"));
    expect(pkg.version).toBe(EXTENSION_VERSION);
    expect(pkg.pi.extensions).toEqual(["./dist/vibeke.js"]);
  });

  test("inert unless VIBEKE=1, socket and token are all set; HERDR_ENV alone is a no-op", async () => {
    for (const env of [
      {},
      { HERDR_ENV: "1" },
      { VIBEKE: "1", VIBEKE_SOCKET: server.path },
      { VIBEKE_SOCKET: server.path, VIBEKE_PANE_TOKEN: "t" },
      { VIBEKE: "1", VIBEKE_PANE_TOKEN: "t", HERDR_ENV: "1" },
    ]) {
      const pi = new FakeHost();
      expect(createExtension(pi as any, { env })).toBeUndefined();
      expect(pi.handlers.size).toBe(0);
    }
    await sleep(50);
    expect(server.msgs.length).toBe(0);
  });

  test("a host that rejects event names does not break loading", () => {
    const pi = {
      on(name: string) {
        if (name.startsWith("session")) throw new Error("unknown event");
      },
    };
    expect(createExtension(pi as any, { env: { VIBEKE: "1", VIBEKE_SOCKET: server.path, VIBEKE_PANE_TOKEN: "t" } })).toBeDefined();
  });

  test("hello then snapshot first on connect", async () => {
    setup(server, "pi");
    await waitFor(() => server.msgs.length >= 2);
    const [a, b] = server.msgs.map((m) => m.msg);
    expect(a.method).toBe("client.hello");
    expect(a.params).toMatchObject({ client: "vibeke-pi-extension", kind: "agent", token: "tok", version: EXTENSION_VERSION });
    expect(b.method).toBe("adapter.signal");
    expect(b.params.event).toBe("Snapshot");
    expect(b.params.harness).toBe("pi");
    expect(b.params.payload).toMatchObject({ is_streaming: false, turn_index: 0, pending_tool_calls: [], open_approvals: [] });
  });
});

describe("event mapping", () => {
  const ctx = () => makeCtx();

  test("pi session: identity, turn, tool, usage, settled", async () => {
    const { pi } = setup(server, "pi");
    const c = ctx();
    await pi.emit("session_start", { reason: "resume" }, c);
    await pi.emit("input", { source: "interactive", text: "x".repeat(300) }, c);
    await pi.emit("agent_start", {}, c);
    await pi.emit("turn_start", {}, c);
    await pi.emit("tool_call", { toolCallId: "t1", toolName: "bash", input: { command: "ls", apiKey: "sek" } }, c);
    await pi.emit("tool_execution_start", { toolCallId: "t1", toolName: "bash", args: { command: "ls" } }, c);
    await pi.emit("tool_execution_end", { toolCallId: "t1", toolName: "bash", isError: false, result: {} }, c);
    await pi.emit(
      "turn_end",
      { message: { usage: { input: 10, output: 5, cacheRead: 3, cacheWrite: 2, cost: { total: 0.01 } } } },
      c,
    );
    await pi.emit(
      "agent_end",
      { messages: [{ role: "user" }, { role: "assistant", stopReason: "stop", content: [{ type: "text", text: "done!" }] }] },
      c,
    );
    await pi.emit("agent_settled", {}, c);
    await pi.emit("session_shutdown", { reason: "quit" }, c);
    await waitFor(() => server.events().includes("SessionEnded"));
    const s = server.signals();
    expect(s.map((x) => x.event)).toEqual([
      "Snapshot",
      "SessionStart",
      "TurnStarted",
      "Working",
      "ToolStarted",
      "ToolEnded",
      "Usage",
      "TurnEnded",
      "SessionEnded",
    ]);
    const by = (e: string) => s.find((x) => x.event === e)!.payload;
    expect(s.every((x) => x.harness === "pi")).toBe(true);
    expect(by("SessionStart")).toMatchObject({
      session_id: "sess-1",
      transcript_path: "/home/u/.pi/agent/sessions/s1.jsonl",
      source: "resume",
      model: "m-1",
      host: "pi",
      host_version: "9.9.9",
      extension_version: EXTENSION_VERSION,
    });
    expect(by("TurnStarted").prompt_preview).toHaveLength(200);
    // The full request goes along (Codex G02 #16): tracking must not treat the preview as it.
    expect(by("TurnStarted").prompt).toBe("x".repeat(300));
    expect(by("TurnStarted").prompt_truncated).toBe(false);
    expect(by("ToolStarted")).toMatchObject({ call_id: "t1", tool: "bash" });
    // input was cached from tool_call (carries the secret key), so it must be redacted
    expect(by("ToolStarted").input).toEqual({ command: "ls", apiKey: "[redacted]" });
    expect(by("ToolEnded")).toEqual(expect.objectContaining({ call_id: "t1", tool: "bash", ok: true }));
    expect(by("ToolEnded").file_path).toBeUndefined();
    expect(by("Usage")).toMatchObject({ input: 10, output: 5, cache_read: 3, cache_write: 2, cost: 0.01 });
    expect(by("TurnEnded")).toMatchObject({ stop_reason: "stop", last_message: "done!" });
    expect(by("SessionEnded").reason).toBe("quit");
  });

  test("TurnStarted carries the full prompt bounded at 8 KiB, flagged when cut", async () => {
    const { pi } = setup(server, "pi");
    const c = ctx();
    const tail = " — and never touch the SSO config";
    const long = "é".repeat(5000) + tail; // 10000+ bytes of UTF-8
    await pi.emit("input", { source: "interactive", text: long }, c);
    await pi.emit("agent_settled", {}, c);
    await pi.emit("input", { source: "interactive", text: "short request" }, c);
    await waitFor(() => server.events().filter((e) => e === "TurnStarted").length === 2);
    const [cut, whole] = server.signals().filter((x) => x.event === "TurnStarted").map((x) => x.payload);
    expect(new TextEncoder().encode(cut.prompt).length).toBeLessThanOrEqual(8 * 1024);
    expect(cut.prompt.length).toBe(4096); // cut on a character boundary, no replacement chars
    expect(cut.prompt).not.toContain("\uFFFD");
    expect(long.startsWith(cut.prompt)).toBe(true);
    expect(cut.prompt_truncated).toBe(true);
    expect(cut.prompt_preview).toHaveLength(200);
    expect(whole).toMatchObject({ prompt: "short request", prompt_truncated: false, prompt_preview: "short request" });
  });

  test("input from an extension is not a user turn; agent_start alone opens one", async () => {
    const { pi } = setup(server, "pi");
    const c = ctx();
    await pi.emit("input", { source: "extension", text: "auto" }, c);
    await pi.emit("agent_start", {}, c);
    await waitFor(() => server.events().includes("Working"));
    const s = server.signals();
    expect(s.map((x) => x.event)).toEqual(["Snapshot", "TurnStarted", "Working"]);
    expect(s[1].payload.prompt_preview).toBeUndefined();
  });

  test("omp: approvals, retries (rate limit), compaction, switch/branch identity, settling", async () => {
    const { pi, h } = setup(server, "omp");
    const c = ctx();
    await pi.emit("session_switch", {}, c);
    await pi.emit("session_branch", {}, c);
    await pi.emit("tool_approval_requested", { toolCallId: "a1", toolName: "bash", reason: "rm", approvalMode: "ask" }, c);
    await pi.emit("tool_approval_resolved", { toolCallId: "a1", approved: false }, c);
    await pi.emit("auto_retry_start", { errorMessage: "429 rate limit exceeded" }, c);
    await pi.emit("auto_retry_start", { errorMessage: "bad input" }, c);
    await pi.emit("auto_retry_end", {}, c);
    await pi.emit("auto_compaction_start", {}, c);
    await pi.emit("auto_compaction_end", {}, c);
    await pi.emit("session_compact", {}, c); // duplicate end: ignored
    await pi.emit("goal_updated", {}, c); // unknown to us: harmless
    await waitFor(() => server.events().filter((e) => e === "Compacting").length >= 2);
    await sleep(30);
    const s = server.signals().slice(1);
    expect(s.map((x) => x.event)).toEqual([
      "SessionStart",
      "SessionStart",
      "ApprovalRequested",
      "ApprovalResolved",
      "Error",
      "Error",
      "Working",
      "Compacting",
      "Compacting",
    ]);
    expect(s[0].payload.source).toBe("switch");
    expect(s[1].payload.source).toBe("branch");
    expect(s[2].payload).toMatchObject({ call_id: "a1", tool: "bash", reason: "rm", approval_mode: "ask" });
    expect(s[3].payload).toMatchObject({ call_id: "a1", approved: false });
    expect(s[4].payload).toMatchObject({ retrying: true, rate_limited: true });
    expect(s[5].payload).toMatchObject({ retrying: true, rate_limited: false });
    expect(s[7].payload.phase).toBe("start");
    expect(s[8].payload.phase).toBe("end");
    expect(h.host).toBe("omp");
    expect(server.signals()[0].harness).toBe("omp");
  });

  test("seq is strictly monotonic across signals", async () => {
    const { pi } = setup(server, "pi");
    const c = ctx();
    for (let i = 0; i < 50; i++) await pi.emit("tool_execution_start", { toolCallId: `c${i}`, toolName: "read", args: {} }, c);
    await waitFor(() => server.signals().length >= 51);
    const seqs = server.signals().map((x) => x.payload.seq as number);
    for (let i = 1; i < seqs.length; i++) expect(seqs[i]).toBeGreaterThan(seqs[i - 1]);
    expect(seqs[0]).toBeGreaterThan(1.7e15); // Date.now()*1000 scale
  });

  test("tool_call always returns undefined; input is cached for tool_execution_end -> file_path", async () => {
    const { pi } = setup(server, "pi");
    const c = ctx();
    expect(await pi.emit("tool_call", { toolCallId: "w", toolName: "write", input: { path: "src/a.ts", content: "x" } }, c)).toBeUndefined();
    expect(await pi.emit("tool_call", { toolCallId: "e", toolName: "edit", input: { path: "/abs/b.ts", edits: [] } }, c)).toBeUndefined();
    expect(await pi.emit("tool_call", { toolCallId: "f", toolName: "write", input: { path: "src/c.ts" } }, c)).toBeUndefined();
    expect(await pi.emit("tool_call", { toolCallId: "r", toolName: "read", input: { path: "src/d.ts" } }, c)).toBeUndefined();
    expect(pi.emitSync("tool_call", { toolCallId: "z", toolName: "write", input: null }, c)).toBeUndefined();
    for (const [id, tool, isError] of [
      ["w", "write", false],
      ["e", "edit", false],
      ["f", "write", true],
      ["r", "read", false],
      ["w", "write", false], // already evicted: no input, no file_path
      ["unseen", "write", false], // loaded mid-call
    ] as const)
      await pi.emit("tool_execution_end", { toolCallId: id, toolName: tool, isError, result: {} }, c);
    await waitFor(() => server.events().filter((e) => e === "ToolEnded").length >= 6);
    const ended = server.signals().filter((x) => x.event === "ToolEnded").map((x) => x.payload);
    expect(ended.map((e) => e.file_path)).toEqual(["/work/proj/src/a.ts", "/abs/b.ts", undefined, undefined, undefined, undefined]);
    expect(ended[2].ok).toBe(false);
  });

  test("redaction: secret keys and 8 KiB truncation", () => {
    expect(redactInput({ a: 1, Authorization: "x", nested: { password: "p", list: [{ my_token: "t" }] } })).toEqual({
      a: 1,
      Authorization: "[redacted]",
      nested: { password: "[redacted]", list: [{ my_token: "[redacted]" }] },
    });
    const big = redactInput({ content: "x".repeat(20000) }) as any;
    expect(big._truncated).toBe(true);
    expect(big.preview.length).toBe(8192);
    const circ: any = { a: 1 };
    circ.self = circ;
    expect(JSON.stringify(redactInput(circ))).toContain("[circular]");
  });
});

describe("turn end handling", () => {
  test("agent_end is debounced 250 ms; agent_start in the window supersedes it", async () => {
    const { pi } = setup(server, "omp");
    const c = makeCtx();
    await pi.emit("agent_start", {}, c);
    await pi.emit("agent_end", { messages: [] }, c);
    await sleep(120);
    expect(server.events()).not.toContain("TurnEnded");
    await pi.emit("agent_start", {}, c); // retry / continuation
    await sleep(350);
    expect(server.events()).not.toContain("TurnEnded");
    await pi.emit("agent_end", { messages: [] }, c);
    await sleep(150);
    expect(server.events()).not.toContain("TurnEnded");
    await waitFor(() => server.events().includes("TurnEnded"), 1000);
    expect(server.events().filter((e) => e === "TurnEnded").length).toBe(1);
  });

  test("agent_settled ends the turn immediately, cancels the debounce, and never double-ends", async () => {
    const { pi } = setup(server, "pi");
    const c = makeCtx();
    await pi.emit("agent_start", {}, c);
    await pi.emit("agent_end", { messages: [] }, c);
    await pi.emit("agent_settled", {}, c);
    await waitFor(() => server.events().includes("TurnEnded"), 200);
    await sleep(350);
    expect(server.events().filter((e) => e === "TurnEnded").length).toBe(1);
  });

  test("omp session_stop is Settling only; it does not end the turn", async () => {
    const { pi } = setup(server, "omp");
    const c = makeCtx();
    await pi.emit("agent_start", {}, c);
    await pi.emit("session_stop", { stop_hook_active: false }, c);
    await sleep(400);
    const ev = server.events();
    expect(ev).toContain("Settling");
    expect(ev).not.toContain("TurnEnded");
    expect(server.signals().find((s) => s.event === "Settling")!.payload.seq).toBeGreaterThan(0);
  });
});

describe("reconnect and fail-open", () => {
  test("snapshot after reconnect reflects live state (pending tools, approvals, streaming)", async () => {
    const { pi } = setup(server, "omp");
    const c = makeCtx();
    await pi.emit("session_start", {}, c);
    await pi.emit("turn_start", {}, c);
    await pi.emit("agent_start", {}, c);
    await pi.emit("tool_execution_start", { toolCallId: "t9", toolName: "bash", args: { command: "sleep 9", token: "s" } }, c);
    await pi.emit("tool_approval_requested", { toolCallId: "a9", toolName: "bash", reason: "r", approvalMode: "m" }, c);
    await waitFor(() => server.events().includes("ApprovalRequested"));
    const before = server.msgs.length;
    await server.kill();
    await server.listen();
    // a signal after the drop triggers/continues reconnecting; wait for the second Snapshot
    await pi.emit("tool_execution_start", { toolCallId: "t10", toolName: "read", args: {} }, c);
    await waitFor(() => server.signals().filter((s) => s.event === "Snapshot").length >= 1 && server.msgs.length > before, 3000, "reconnect");
    const first = server.msgs.slice(before).map((m) => m.msg);
    expect(first[0].method).toBe("client.hello");
    expect(first[1].params.event).toBe("Snapshot");
    const snap = first[1].params.payload;
    expect(snap).toMatchObject({
      session_id: "sess-1",
      session_file: "/home/u/.pi/agent/sessions/s1.jsonl",
      is_streaming: true,
      turn_index: 1,
      model: "m-1",
    });
    expect(snap.pending_tool_calls.map((p: any) => p.call_id).sort()).toEqual(["t10", "t9"]);
    expect(snap.pending_tool_calls.find((p: any) => p.call_id === "t9").input).toEqual({ command: "sleep 9", token: "[redacted]" });
    expect(snap.open_approvals).toEqual([{ call_id: "a9", tool: "bash", reason: "r" }]);
    expect(snap.seq).toBeGreaterThan(0);
  });

  test("a dropped (overflowed) queue still yields a correct snapshot", async () => {
    await server.kill(); // server is down from the start
    const { pi, h } = setup(server, "pi", { clientOverrides: { backoffMinMs: 5, backoffMaxMs: 20, maxTries: 2 } });
    const c = makeCtx();
    await pi.emit("agent_start", {}, c);
    await pi.emit("turn_start", {}, c);
    await pi.emit("tool_execution_start", { toolCallId: "live", toolName: "bash", args: { command: "x" } }, c);
    for (let i = 0; i < 700; i++) await pi.emit("turn_end", { message: { usage: { input: 1, output: 1 } } }, c);
    expect(h.client.dropped).toBeGreaterThan(0);
    await server.listen();
    await sleep(80); // let the failed burst expire
    await pi.emit("auto_retry_end", {}, c); // any signal starts a new connect burst
    await waitFor(() => server.events().includes("Snapshot"), 3000, "snapshot");
    const snap = server.signals().find((s) => s.event === "Snapshot")!.payload;
    expect(snap.is_streaming).toBe(true);
    expect(snap.turn_index).toBe(1);
    expect(snap.pending_tool_calls.map((p: any) => p.call_id)).toEqual(["live"]);
    // the stale queued history is not replayed ahead of the snapshot
    expect(server.events()[0]).toBe("Snapshot");
  });

  test("server down: handlers never block the host (<= 0.5 ms) and never throw", async () => {
    await server.kill();
    const { pi } = setup(server, "pi");
    const c = makeCtx();
    const n = 300;
    const t0 = performance.now();
    for (let i = 0; i < n; i++) {
      pi.emitSync("tool_call", { toolCallId: `c${i}`, toolName: "write", input: { path: "a" } }, c);
      pi.emitSync("tool_execution_start", { toolCallId: `c${i}`, toolName: "write", args: { path: "a" } }, c);
      pi.emitSync("tool_execution_end", { toolCallId: `c${i}`, toolName: "write", isError: false }, c);
    }
    const perEvent = (performance.now() - t0) / (n * 3);
    expect(perEvent).toBeLessThan(0.5);
  });

  test("server killed mid-run: later events do not throw and the extension reconnects", async () => {
    const { pi, h } = setup(server, "pi");
    const c = makeCtx();
    await pi.emit("agent_start", {}, c);
    await waitFor(() => server.events().includes("Working"));
    await server.kill();
    for (let i = 0; i < 20; i++) await pi.emit("turn_end", { message: { usage: { input: 1, output: 1 } } }, c);
    await waitFor(() => !h.client.connected);
  });
});

describe("uiContext wrapper", () => {
  async function wrapUi(host: "pi" | "omp" = "pi") {
    const s = setup(server, host);
    const { ui, calls } = fakeUi();
    const c = makeCtx({ ui });
    await s.pi.emit("session_start", {}, c);
    return { ...s, ui, calls, c };
  }

  test("native answer first: returned as-is, Vibeke gate closed, DialogResolved(native) sent", async () => {
    const { ui, calls } = await wrapUi();
    const p = ui.confirm("Allow rm?", "rm -rf x");
    await waitFor(() => server.gates.length === 1);
    expect(server.gates[0].params).toMatchObject({
      harness: "pi",
      event: "Dialog",
      payload: { method: "confirm", title: "Allow rm?", message: "rm -rf x" },
    });
    expect(server.gates[0].params.payload.dialog_id).toBeTruthy();
    calls[0].resolve(true);
    expect(await p).toBe(true);
    await waitFor(() => server.closedConns.has(server.gates[0].conn));
    await waitFor(() => server.events().includes("DialogResolved"));
    const r = server.signals().find((x) => x.event === "DialogResolved")!.payload;
    expect(r).toMatchObject({ dialog_id: server.gates[0].params.payload.dialog_id, by: "native", value: true });
    expect(calls[0].aborts).toBe(0);
  });

  test("Vibeke answer first: native dialog aborted exactly once, caller gets the Vibeke value", async () => {
    const { ui, calls } = await wrapUi();
    const p = ui.select("Pick", ["Allow", "Deny"]);
    await waitFor(() => server.gates.length === 1);
    expect(server.gates[0].params.payload).toMatchObject({ method: "select", options: ["Allow", "Deny"] });
    server.respondGate(0, { value: "Deny" });
    expect(await p).toBe("Deny");
    await sleep(30);
    expect(calls[0].aborts).toBe(1);
    expect(calls[0].signal!.aborted).toBe(true);
  });

  test("confirm and input get typed Vibeke answers; invalid values are ignored", async () => {
    const { ui, calls } = await wrapUi();
    const p1 = ui.confirm("ok?", "m");
    await waitFor(() => server.gates.length === 1);
    server.respondGate(0, { value: false });
    expect(await p1).toBe(false);

    const p2 = ui.input("name?", "ph");
    await waitFor(() => server.gates.length === 2);
    server.respondGate(1, { value: "bob" });
    expect(await p2).toBe("bob");

    const p3 = ui.select("pick", ["a", "b"]);
    await waitFor(() => server.gates.length === 3);
    server.respondGate(2, { value: "zzz" }); // not an option
    await sleep(40);
    expect(calls[2].aborts).toBe(0);
    calls[2].resolve("a");
    expect(await p3).toBe("a");
  });

  test("Vibeke answer applied: delivery ack with the gate's interaction and key on the main connection", async () => {
    const { ui } = await wrapUi();
    const p = ui.select("Pick", ["Allow", "Deny"]);
    await waitFor(() => server.gates.length === 1);
    server.respondGate(0, { value: "Deny" }, { interaction: "int-7", idempotency_key: "int-7:2" });
    expect(await p).toBe("Deny");
    await waitFor(() => server.calls("adapter.delivery_ack").length === 1, 3000, "delivery ack");
    const ack = server.calls("adapter.delivery_ack")[0];
    expect(ack.msg.params).toEqual({ interaction: "int-7", idempotency_key: "int-7:2", applied: true });
    // Sent over the extension's main connection (the one carrying signals), with the pane token.
    const main = server.calls("adapter.signal")[0].conn;
    expect(ack.conn).toBe(main);
    expect(ack.conn).not.toBe(server.gates[0].conn);
    expect(server.hello(ack.conn).token).toBe("tok");
  });

  test("native answer first: no delivery ack", async () => {
    const { ui, calls, h } = await wrapUi();
    const p = ui.confirm("t", "m");
    await waitFor(() => server.gates.length === 1);
    calls[0].resolve(true);
    expect(await p).toBe(true);
    await sleep(40);
    expect(server.calls("adapter.delivery_ack")).toHaveLength(0);
    expect(h.client.pendingAcks).toBe(0);
  });

  test("an unanswered delivery ack is re-sent after the main connection reconnects", async () => {
    const { ui, pi, c, h } = await wrapUi();
    server.noReply.add("adapter.delivery_ack");
    const p = ui.confirm("t", "m");
    await waitFor(() => server.gates.length === 1);
    server.respondGate(0, { value: true }, { interaction: "int-1", idempotency_key: "int-1:1" });
    expect(await p).toBe(true);
    await waitFor(() => server.calls("adapter.delivery_ack").length === 1);
    expect(h.client.pendingAcks).toBe(1);
    server.noReply.clear();
    await server.kill();
    await server.listen();
    await pi.emit("agent_start", {}, c); // any signal drives the reconnect
    await waitFor(() => server.calls("adapter.delivery_ack").length === 2, 3000, "re-sent ack");
    const again = server.calls("adapter.delivery_ack")[1];
    expect(again.msg.params).toEqual({ interaction: "int-1", idempotency_key: "int-1:1", applied: true });
    expect(server.hello(again.conn).token).toBe("tok");
    await waitFor(() => h.client.pendingAcks === 0, 3000, "ack answered");
  });

  test("gate connection dropped while the dialog is pending: re-issued with the same dialog_id", async () => {
    const { ui, calls, h } = await wrapUi();
    const p = ui.confirm("Allow rm?", "rm -rf x");
    await waitFor(() => server.gates.length === 1);
    const first = server.gates[0];
    server.dropGate(0);
    await waitFor(() => server.gates.length === 2, 3000, "gate re-issued");
    const second = server.gates[1];
    expect(second.conn).not.toBe(first.conn);
    expect(second.params).toEqual(first.params); // same payload, so the server re-attaches by native ref
    expect(second.params.payload.dialog_id).toBe(first.params.payload.dialog_id);
    expect(server.hello(second.conn).token).toBe("tok");
    expect(h.client.gateReconnects).toBe(1);
    expect(calls[0].aborts).toBe(0); // the native dialog stayed up throughout
    server.respondGate(1, { value: true }, { interaction: "int-2", idempotency_key: "int-2:1" });
    expect(await p).toBe(true);
    expect(calls[0].aborts).toBe(1);
    await waitFor(() => server.calls("adapter.delivery_ack").length === 1);
    // Settled: a later drop does not reconnect again.
    await sleep(60);
    expect(server.gates).toHaveLength(2);
  });

  test("snapshot after reconnect lists pending wrapper dialogs (and drops resolved ones)", async () => {
    const { ui, calls, pi, c } = await wrapUi();
    const p = ui.select("Pick a branch", ["main", "dev"]);
    await waitFor(() => server.gates.length === 1);
    const dialogId = server.gates[0].params.payload.dialog_id;
    await server.kill();
    await server.listen();
    await pi.emit("agent_start", {}, c);
    const snapsBefore = server.signals().filter((s) => s.event === "Snapshot").length;
    await waitFor(() => server.signals().filter((s) => s.event === "Snapshot").length > snapsBefore, 3000, "snapshot");
    const snap = server.signals().filter((s) => s.event === "Snapshot").at(-1)!.payload;
    expect(snap.pending_dialogs).toEqual([
      { method: "select", title: "Pick a branch", dialog_id: dialogId, options: ["main", "dev"] },
    ]);
    // The gate came back too, for the same dialog.
    await waitFor(() => server.gates.some((g) => g.params.payload.dialog_id === dialogId), 3000, "gate back");
    calls[0].resolve("dev");
    expect(await p).toBe("dev");
    const snaps = () => server.signals().filter((s) => s.event === "Snapshot");
    const before = snaps().length;
    await server.kill();
    await server.listen();
    await pi.emit("agent_start", {}, c);
    await waitFor(() => snaps().length > before, 3000, "snapshot 2");
    expect(snaps().at(-1)!.payload.pending_dialogs).toEqual([]);
  });

  test("a null decision keeps waiting for the native dialog", async () => {
    const { ui, calls } = await wrapUi();
    const p = ui.confirm("t", "m");
    await waitFor(() => server.gates.length === 1);
    server.respondGate(0, null);
    await sleep(40);
    expect(calls[0].aborts).toBe(0);
    calls[0].resolve(true);
    expect(await p).toBe(true);
  });

  test("Vibeke unreachable: just the native dialog", async () => {
    const { ui, calls } = await wrapUi();
    await server.kill();
    const p = ui.confirm("t", "m");
    await sleep(40);
    calls[0].resolve(false);
    expect(await p).toBe(false);
    expect(calls[0].aborts).toBe(0);
  });

  test("caller timeout is preserved and caller abort dismisses the native dialog", async () => {
    const { ui, calls } = await wrapUi();
    const caller = new AbortController();
    const p = ui.confirm("t", "m", { timeout: 1234, signal: caller.signal });
    await waitFor(() => calls.length === 1);
    expect(calls[0].args[2].timeout).toBe(1234);
    expect(calls[0].signal).not.toBe(caller.signal); // linked, not the same
    caller.abort();
    expect(await p).toBeUndefined();
    expect(calls[0].aborts).toBe(1);
    // already-aborted caller signal
    const pre = new AbortController();
    pre.abort();
    const p2 = ui.select("t", ["a"], { signal: pre.signal });
    await waitFor(() => calls.length === 2);
    expect(calls[1].signal!.aborted).toBe(true);
    expect(await p2).toBeUndefined();
  });

  test("two concurrent dialogs are independent", async () => {
    const { ui, calls } = await wrapUi();
    const p1 = ui.confirm("one", "m");
    const p2 = ui.confirm("two", "m");
    await waitFor(() => server.gates.length === 2);
    const idx = server.gates.findIndex((g) => g.params.payload.title === "two");
    server.respondGate(idx, { value: true });
    expect(await p2).toBe(true);
    expect(calls[1].aborts).toBe(1);
    expect(calls[0].aborts).toBe(0);
    calls[0].resolve(false);
    expect(await p1).toBe(false);
  });

  test("wrapping is idempotent (WeakSet) and survives a uiContext swap", async () => {
    const { pi, h, ui, calls } = await wrapUi();
    const first = ui.confirm;
    await pi.emit("session_start", {}, makeCtx({ ui }));
    expect(ui.confirm).toBe(first);

    const swapped = fakeUi();
    // a signal-triggering event with the *new* ui in ctx re-wraps it
    await pi.emit("agent_start", {}, makeCtx({ ui: swapped.ui }));
    expect(swapped.ui.confirm).not.toBe(swapped.calls); // sanity
    const p = swapped.ui.confirm("t", "m");
    await waitFor(() => server.gates.length === 1);
    server.respondGate(0, { value: true });
    expect(await p).toBe(true);
    expect(swapped.calls[0].aborts).toBe(1);
    expect(h.wrapperEnabled()).toBe(true);
    void calls;
  });

  test("not wrapped outside TUI mode; self-check disables the wrapper on a bad shape", async () => {
    const rpc = setup(server, "pi");
    const a = fakeUi();
    const orig = a.ui.confirm;
    await rpc.pi.emit("session_start", {}, makeCtx({ ui: a.ui, mode: "rpc" }));
    expect(a.ui.confirm).toBe(orig);

    const bad = setup(server, "pi");
    const f = Object.freeze(fakeUi().ui);
    await bad.pi.emit("session_start", {}, makeCtx({ ui: f }));
    expect(bad.h.wrapperEnabled()).toBe(false);

    const missing = setup(server, "pi");
    await missing.pi.emit("session_start", {}, makeCtx({ ui: { confirm() {} } }));
    expect(missing.h.wrapperEnabled()).toBe(false);
  });

  test("headless owner: wrapper off, only identity and file changes are reported", async () => {
    const pi = new FakeHost();
    createExtension(pi as any, {
      env: { VIBEKE: "1", VIBEKE_SOCKET: server.path, VIBEKE_PANE_TOKEN: "t", VIBEKE_HEADLESS_OWNER: "1" },
      host: "pi",
    });
    const a = fakeUi();
    const orig = a.ui.confirm;
    const c = makeCtx({ ui: a.ui });
    await pi.emit("session_start", {}, c);
    await pi.emit("agent_start", {}, c);
    await pi.emit("tool_call", { toolCallId: "w", toolName: "write", input: { path: "f" } }, c);
    await pi.emit("tool_execution_end", { toolCallId: "w", toolName: "write", isError: false }, c);
    await waitFor(() => server.events().includes("ToolEnded"));
    expect(server.events()).toEqual(["Snapshot", "SessionStart", "ToolEnded"]);
    expect(a.ui.confirm).toBe(orig);
  });
});

describe("control channel", () => {
  const models = [
    { id: "claude-x", name: "Claude X", provider: "anthropic" },
    { id: "gpt-y", name: "GPT Y", provider: "openai" },
  ];
  function ctxWithModels(over: Record<string, unknown> = {}) {
    return makeCtx({
      model: models[0],
      modelRegistry: {
        getAvailable: () => models,
        find: (p: string, id: string) => models.find((m) => m.provider === p && m.id === id),
      },
      ...over,
    });
  }
  const replies = () => server.calls("adapter.control").map((m) => m.msg.params.reply).filter(Boolean);

  test("lists and switches models, lists commands, refuses unknown models", async () => {
    server.control = [];
    const { pi } = setup(server, "pi", { control: true });
    const set: any[] = [];
    (pi as any).setModel = async (m: any) => {
      set.push(m);
      return true;
    };
    (pi as any).getCommands = () => [{ name: "review-pr", description: "Prompt template", source: "prompt" }];
    await pi.emit("session_start", {}, ctxWithModels());
    await waitFor(() => server.calls("adapter.control").length >= 1, 3000, "first poll");
    const first = server.calls("adapter.control")[0].msg.params;
    expect(first.ops).toEqual(["models", "set_model", "commands"]);
    expect(server.hello(server.calls("adapter.control")[0].conn).token).toBe("tok");

    server.pushControl({ id: "c1", op: "models", params: {} });
    await waitFor(() => replies().length >= 1, 3000, "models reply");
    expect(replies()[0]).toEqual({
      id: "c1",
      ok: true,
      result: {
        models: [
          { id: "anthropic/claude-x", label: "Claude X", description: "anthropic", current: true },
          { id: "openai/gpt-y", label: "GPT Y", description: "openai", current: false },
        ],
      },
    });

    server.pushControl({ id: "c2", op: "set_model", params: { model: "openai/gpt-y", scope: "session" } });
    await waitFor(() => replies().length >= 2, 3000, "set_model reply");
    expect(replies()[1]).toEqual({ id: "c2", ok: true, result: { model: "openai/gpt-y", default_changed: true } });
    expect(set.map((m) => m.id)).toEqual(["gpt-y"]);

    server.pushControl({ id: "c3", op: "models", params: {} });
    await waitFor(() => replies().length >= 3, 3000, "models after switch");
    expect(replies()[2].result.models.find((m: any) => m.current).id).toBe("openai/gpt-y");

    server.pushControl({ id: "c4", op: "set_model", params: { model: "nope/none" } });
    await waitFor(() => replies().length >= 4, 3000, "refusal");
    expect(replies()[3]).toMatchObject({ id: "c4", ok: false });
    expect(replies()[3].error).toContain("unknown model");

    server.pushControl({ id: "c5", op: "commands", params: {} });
    await waitFor(() => replies().length >= 5, 3000, "commands");
    expect(replies()[4].result).toEqual({ commands: [{ name: "review-pr", description: "Prompt template" }] });
  });

  test("omp keeps the switch to the session and refuses scope default", async () => {
    server.control = [];
    const { pi } = setup(server, "omp", { control: true });
    (pi as any).setModel = async () => true;
    await pi.emit("session_start", {}, ctxWithModels());
    server.pushControl({ id: "c1", op: "set_model", params: { model: "openai/gpt-y", scope: "default" } });
    server.pushControl({ id: "c2", op: "set_model", params: { model: "openai/gpt-y", scope: "session" } });
    await waitFor(() => replies().length >= 2, 3000, "replies");
    expect(replies()[0]).toMatchObject({ id: "c1", ok: false });
    expect(replies()[1]).toEqual({ id: "c2", ok: true, result: { model: "openai/gpt-y", default_changed: false } });
  });

  test("a server without the channel ends the loop; non-TUI modes never poll", async () => {
    const { pi } = setup(server, "pi", { control: true });
    await pi.emit("session_start", {}, ctxWithModels({ mode: "rpc" }));
    await sleep(100);
    expect(server.calls("adapter.control").length).toBe(0);
    await pi.emit("session_switch", {}, ctxWithModels());
    await waitFor(() => server.calls("adapter.control").length >= 1, 3000, "poll");
    await sleep(300);
    expect(server.calls("adapter.control").length).toBe(1);
  });
});
