// Unit test against an in-process mock of the server's wire behaviour. Run: `node --test test/`
// (Node >= 22.18 strips the types) or `bun test`.
import assert from "node:assert/strict";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { after, before, test } from "node:test";
import { EventOverflow, VibekeClient, VibekeError } from "../src/index.ts";

const cursor = { machine_uuid: "m", session_uuid: "s", log_epoch: "e", seq: 7 };
let dir: string;
let server: net.Server;
let sock: string;
let conns: net.Socket[] = [];

function reply(c: net.Socket, id: unknown, result: unknown): void {
  c.write(JSON.stringify({ jsonrpc: "2.0", id, result }) + "\n");
}

before(async () => {
  dir = fs.mkdtempSync(path.join(os.tmpdir(), "vkts-"));
  sock = path.join(dir, "s.sock");
  server = net.createServer((c) => {
    conns.push(c);
    let buf = "";
    c.on("data", (d) => {
      buf += d.toString("utf8");
      let i;
      while ((i = buf.indexOf("\n")) >= 0) {
        const req = JSON.parse(buf.slice(0, i));
        buf = buf.slice(i + 1);
        handle(c, req);
      }
    });
  });
  await new Promise<void>((r) => server.listen(sock, r));
});

after(() => {
  for (const c of conns) c.destroy();
  server.close();
  fs.rmSync(dir, { recursive: true, force: true });
});

const held: { id: unknown }[] = [];
function handle(c: net.Socket, req: any): void {
  switch (req.method) {
    case "client.hello":
      return reply(c, req.id, {
        server_version: "t", api: "vibeke/1", session: "default", machine: "local",
        capabilities: ["*"], features: [],
      });
    case "server.status":
      // Answer a later request first: responses may arrive out of order.
      if (req.params.__hold) return void held.push({ id: req.id });
      reply(c, req.id, { pid: 1, version: "t x", uptime_ms: 5, session: "default", machine: "local", panes: 0, holders: { live: 0 }, clients: 1, event_seq: 3 });
      for (const h of held.splice(0)) reply(c, h.id, { pid: 2 });
      return;
    case "pane.close":
      return void c.write(JSON.stringify({ jsonrpc: "2.0", id: req.id, error: { code: -32001, message: "no such pane", data: { kind: "not_found", details: { object: "pane" }, retryable: false } } }) + "\n");
    case "events.subscribe": {
      // The first push is in the same write as the response: it must not be lost.
      const ev = (seq: number) => ({ jsonrpc: "2.0", method: "events.event", params: { subscription_id: "s1", event: { seq, ts: 1, v: 1, tier: "sync", type: "workspace.created", subject: { workspace: "w" }, actor: { kind: "system" }, data: {} } } });
      c.write(
        JSON.stringify({ jsonrpc: "2.0", id: req.id, result: { subscription_id: "s1", at: cursor } }) + "\n" +
          JSON.stringify(ev(8)) + "\n" +
          JSON.stringify({ jsonrpc: "2.0", method: "unknown.notification", params: {} }) + "\n" +
          JSON.stringify(ev(9)) + "\n",
      );
      if (req.params.types?.includes("overflow")) {
        setTimeout(() => c.write(JSON.stringify({ jsonrpc: "2.0", method: "events.overflow", params: { subscription_id: "s1", resume_from: { ...cursor, seq: 99 } } }) + "\n"), 20);
      }
      return;
    }
  }
}

test("hello, typed call, U+2028 inside a string, pipelining", async () => {
  const c = await VibekeClient.connect({ socketPath: sock, token: "" });
  assert.equal(c.hello?.api, "vibeke/1");
  const [a, b] = await Promise.all([c.call("server.status", { __hold: true } as any), c.call("server.status", {})]);
  assert.equal(b.version, "t x");
  assert.equal((a as any).pid, 2);
  c.close();
});

test("error responses reject with the stable kind", async () => {
  const c = await VibekeClient.connect({ socketPath: sock, hello: false });
  await assert.rejects(c.call("pane.close", { pane: "p9" }), (e: unknown) => {
    assert.ok(e instanceof VibekeError);
    assert.equal(e.kind, "not_found");
    assert.equal(e.code, -32001);
    assert.equal(e.retryable, false);
    return true;
  });
  c.close();
});

test("events are a typed async iterator; unknown notifications are ignored", async () => {
  const c = await VibekeClient.connect({ socketPath: sock, hello: false });
  const s = await c.events({ types: ["workspace.*"] });
  assert.equal(s.at.seq, 7);
  const seqs: number[] = [];
  for await (const e of s) {
    seqs.push(e.seq);
    if (seqs.length === 2) break; // `return()` closes the stream
  }
  assert.deepEqual(seqs, [8, 9]);
  c.close();
});

test("overflow ends the stream with the resume cursor; closing the socket ends streams", async () => {
  const c = await VibekeClient.connect({ socketPath: sock, hello: false });
  const s = await c.events({ types: ["overflow"] });
  const seen: number[] = [];
  await assert.rejects(
    (async () => {
      for await (const e of s) seen.push(e.seq);
    })(),
    (e: unknown) => e instanceof EventOverflow && e.resumeFrom.seq === 99,
  );
  assert.deepEqual(seen, [8, 9]);
  const s2 = await c.events({});
  c.close();
  const rest: number[] = [];
  for await (const e of s2) rest.push(e.seq);
  assert.deepEqual(rest, [8, 9]);
  await assert.rejects(c.call("server.status", {}), /connection closed/);
});
