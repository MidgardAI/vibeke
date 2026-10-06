// Unit test against an in-process mock of the server's wire behaviour. Run: `node --test test/`
// (Node >= 22.18 strips the types) or `bun test`.
import assert from "node:assert/strict";
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { after, before, test } from "node:test";
import { EventOverflow, SocketTrustError, VibekeClient, VibekeError, checkSocketTrust } from "../src/index.ts";

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
    c.on("error", () => {}); // a client may close while the mock still writes
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
const unsubscribed: string[] = [];
let lastConn: net.Socket | null = null;
const evLine = (sid: string, seq: number) =>
  JSON.stringify({ jsonrpc: "2.0", method: "events.event", params: { subscription_id: sid, event: { seq, ts: 1, v: 1, tier: "sync", type: "workspace.created", subject: { workspace: "w" }, actor: { kind: "system" }, data: {} } } }) + "\n";
function handle(c: net.Socket, req: any): void {
  lastConn = c;
  switch (req.method) {
    case "events.unsubscribe":
      unsubscribed.push(req.params.subscription_id);
      return reply(c, req.id, { unsubscribed: true });
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
      if (req.params.types?.includes("burst")) {
        // Response, events and the overflow all in ONE write (finding 9).
        c.write(
          JSON.stringify({ jsonrpc: "2.0", id: req.id, result: { subscription_id: "sb", at: cursor } }) + "\n" +
            evLine("sb", 10) + evLine("sb", 11) +
            JSON.stringify({ jsonrpc: "2.0", method: "events.overflow", params: { subscription_id: "sb", resume_from: { ...cursor, seq: 11 } } }) + "\n",
        );
        return;
      }
      if (req.params.types?.includes("closing")) {
        return reply(c, req.id, { subscription_id: "sc", at: cursor });
      }
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

test("response, events and overflow in one chunk: the events, then EventOverflow", async () => {
  const c = await VibekeClient.connect({ socketPath: sock, hello: false });
  const s = await c.events({ types: ["burst"] });
  const seen: number[] = [];
  const run = (async () => {
    for await (const e of s) seen.push(e.seq);
  })();
  let timer: NodeJS.Timeout | undefined;
  const timeout = new Promise((_, rej) => (timer = setTimeout(() => rej(new Error("hung: overflow lost")), 2000)));
  await assert.rejects(Promise.race([run, timeout]), (e: unknown) => e instanceof EventOverflow && e.resumeFrom.seq === 11);
  clearTimeout(timer);
  assert.deepEqual(seen, [10, 11]);
  assert.deepEqual(c.retained(), { streams: 0, events: 0 });
  c.close();
});

test("closing a stream unsubscribes and drops later events (bounded retention)", async () => {
  const c = await VibekeClient.connect({ socketPath: sock, hello: false });
  const s = await c.events({ types: ["closing"] });
  const conn = lastConn!;
  s.close();
  // The server keeps sending (it has not processed the unsubscribe yet): 1,000 events.
  let burst = "";
  for (let i = 0; i < 1000; i++) burst += evLine("sc", 100 + i);
  conn.write(burst);
  // A round trip after the burst: every event above has been handled.
  const st = await c.call("server.status", {});
  assert.equal(st.session, "default");
  assert.ok(unsubscribed.includes("sc"), "events.unsubscribe was sent");
  assert.deepEqual(c.retained(), { streams: 0, events: 0 });
  assert.equal(s.queued, 0);
  assert.equal((c as any).early, undefined, "no early-event buffer");
  const it = s[Symbol.asyncIterator]();
  assert.deepEqual(await it.next(), { value: undefined, done: true });
  c.close();
});

// ---- socket trust (finding 2) ----

function listener(p: string): Promise<{ srv: net.Server; conns: () => number; bytes: () => number }> {
  let conns = 0;
  let bytes = 0;
  const srv = net.createServer((c) => {
    conns++;
    c.on("data", (d) => (bytes += d.length));
  });
  return new Promise((r) => srv.listen(p, () => r({ srv, conns: () => conns, bytes: () => bytes })));
}

function tmp(): string {
  return fs.mkdtempSync(path.join(os.tmpdir(), "vkt-"));
}

test("checkSocketTrust mirrors the CLI: owner, symlinks, modes, file type", async () => {
  const base = tmp();
  try {
    const root = path.join(base, "run");
    fs.mkdirSync(path.join(root, "default"), { recursive: true, mode: 0o700 });
    fs.chmodSync(root, 0o700);
    fs.chmodSync(path.join(root, "default"), 0o700);
    const sockPath = path.join(root, "default", "vibeke.sock");
    const l = await listener(sockPath);
    // OK as is, and missing pieces pass.
    checkSocketTrust(sockPath, { root });
    checkSocketTrust(path.join(root, "other", "vibeke.sock"), { root });
    // Foreign owner (the uid override plays the other user).
    assert.throws(() => checkSocketTrust(sockPath, { root, uid: 4242 }), (e: unknown) => e instanceof SocketTrustError && /owned by uid/.test(e.message));
    // Mode != 0700 under the root.
    fs.chmodSync(path.join(root, "default"), 0o750);
    assert.throws(() => checkSocketTrust(sockPath, { root }), /need 700/);
    fs.chmodSync(path.join(root, "default"), 0o700);
    // Not a socket.
    const plain = path.join(root, "default", "plain.sock");
    fs.writeFileSync(plain, "");
    assert.throws(() => checkSocketTrust(plain, { root }), /not a socket/);
    // Symlinked session dir.
    const elsewhere = path.join(base, "elsewhere");
    fs.mkdirSync(elsewhere, { mode: 0o700 });
    const l2 = await listener(path.join(elsewhere, "vibeke.sock"));
    fs.symlinkSync(elsewhere, path.join(root, "linked"));
    assert.throws(() => checkSocketTrust(path.join(root, "linked", "vibeke.sock"), { root }), /not a plain directory/);
    // Symlinked runtime root.
    const rootLink = path.join(base, "rootlink");
    fs.symlinkSync(root, rootLink);
    assert.throws(() => checkSocketTrust(path.join(rootLink, "default", "vibeke.sock"), { root: rootLink }), /not a plain directory/);
    // Explicit path elsewhere: group/world-writable parent refused, 0755 fine.
    const shared = path.join(base, "shared");
    fs.mkdirSync(shared);
    const l3 = await listener(path.join(shared, "x.sock"));
    fs.chmodSync(shared, 0o777);
    assert.throws(() => checkSocketTrust(path.join(shared, "x.sock"), { root }), /mode 777/);
    fs.chmodSync(shared, 0o775);
    assert.throws(() => checkSocketTrust(path.join(shared, "x.sock"), { root }), /mode 775/);
    fs.chmodSync(shared, 0o755);
    checkSocketTrust(path.join(shared, "x.sock"), { root });
    for (const x of [l, l2, l3]) x.srv.close();
  } finally {
    fs.rmSync(base, { recursive: true, force: true });
  }
});

test("connect refuses an untrusted socket before sending a byte; insecure bypasses", async () => {
  const base = tmp();
  const saved = process.env.VIBEKE_RUNTIME_DIR;
  try {
    // Symlinked session dir under the runtime root, holding a real listening socket.
    const root = path.join(base, "run");
    fs.mkdirSync(root, { mode: 0o700 });
    fs.chmodSync(root, 0o700);
    const planted = path.join(base, "planted");
    fs.mkdirSync(planted, { mode: 0o700 });
    const l1 = await listener(path.join(planted, "vibeke.sock"));
    fs.symlinkSync(planted, path.join(root, "default"));
    process.env.VIBEKE_RUNTIME_DIR = root;
    await assert.rejects(
      VibekeClient.connect({ session: "default", token: "secret-pane-token" }),
      (e: unknown) => e instanceof SocketTrustError,
    );
    // Explicit socket in a world-writable directory.
    const shared = path.join(base, "shared");
    fs.mkdirSync(shared);
    fs.chmodSync(shared, 0o777);
    const l2 = await listener(path.join(shared, "x.sock"));
    await assert.rejects(
      VibekeClient.connect({ socketPath: path.join(shared, "x.sock"), token: "secret-pane-token" }),
      (e: unknown) => e instanceof SocketTrustError,
    );
    await new Promise((r) => setTimeout(r, 50));
    for (const l of [l1, l2]) {
      assert.equal(l.conns(), 0, "no connection");
      assert.equal(l.bytes(), 0, "zero bytes sent");
    }
    // insecure: connects (and would send).
    const c = await VibekeClient.connect({ socketPath: path.join(shared, "x.sock"), hello: false, insecure: true });
    await new Promise((r) => setTimeout(r, 50));
    assert.equal(l2.conns(), 1);
    c.close();
    l1.srv.close();
    l2.srv.close();
  } finally {
    if (saved === undefined) delete process.env.VIBEKE_RUNTIME_DIR;
    else process.env.VIBEKE_RUNTIME_DIR = saved;
    fs.rmSync(base, { recursive: true, force: true });
  }
});
