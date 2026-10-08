// bun web/packages/core/scripts/check-relay.ts [wss://relay.vibeke.dev] [expected origin]
// Uses fresh test identities. No local host configuration or pairing records are changed.
import assert from 'node:assert/strict';
import { generateKeyPairSync, randomBytes, sign } from 'node:crypto';
import { hostId, x25519Public } from '../src/keys';
import { Initiator, Responder } from '../src/noise';

const endpoint = new URL(process.argv[2] ?? 'wss://relay.vibeke.dev');
assert(['ws:', 'wss:'].includes(endpoint.protocol), 'Use a ws:// or wss:// endpoint');
const origin = process.argv[3] ?? endpoint.origin.replace(/^ws/, 'http');
const sockets: WebSocket[] = [];
const deadline = setTimeout(() => {
  console.error('Relay check timed out');
  process.exit(1);
}, 30_000);

async function connect(path: string) {
  const ws = new WebSocket(new URL(path, endpoint));
  sockets.push(ws);
  ws.binaryType = 'arraybuffer';
  const queue: Array<string | ArrayBuffer> = [];
  let waiter: { resolve: (data: string | ArrayBuffer) => void; reject: (e: Error) => void } | null = null;
  let failure: Error | null = null;
  ws.addEventListener('message', (event) => {
    if (waiter) {
      const pending = waiter;
      waiter = null;
      pending.resolve(event.data);
    } else queue.push(event.data);
  });
  const fail = (error: Error) => {
    failure = error;
    waiter?.reject(error);
    waiter = null;
  };
  ws.addEventListener('close', (event) => fail(new Error(`WebSocket closed: ${event.code}`)));
  ws.addEventListener('error', () => fail(new Error('WebSocket connection failed')));
  await new Promise<void>((resolve, reject) => {
    ws.addEventListener('open', () => resolve(), { once: true });
    ws.addEventListener('error', () => reject(new Error('Cannot open WebSocket')), { once: true });
  });
  const next = async (): Promise<string | ArrayBuffer> => {
    if (queue.length) return queue.shift()!;
    if (failure) throw failure;
    assert.equal(waiter, null, 'Only one pending read per socket');
    return new Promise((resolve, reject) => { waiter = { resolve, reject }; });
  };
  return {
    ws,
    async json() {
      const message = await next();
      assert.equal(typeof message, 'string');
      return JSON.parse(message as string);
    },
    async binary() {
      const message = await next();
      assert(message instanceof ArrayBuffer);
      return new Uint8Array(message);
    },
  };
}

try {
  const { publicKey, privateKey } = generateKeyPairSync('ed25519');
  const publicBytes = publicKey.export({ format: 'der', type: 'spki' }).subarray(-32);
  const host = hostId(publicBytes);
  const statusUrl = new URL(`/v1/status?host=${host}`, origin);
  try {
    const st = (await (await fetch(statusUrl)).json()) as { auth?: unknown };
    console.log(`relay auth: ${JSON.stringify(st.auth ?? null)}`);
  } catch (e) {
    console.log(`relay auth: unavailable (${(e as Error).message})`);
  }
  const control = await connect(process.env.VIBEKE_HOST_TOKEN ? `/v1/host?token=${encodeURIComponent(process.env.VIBEKE_HOST_TOKEN)}` : '/v1/host');
  const challenge = await control.json();
  assert.equal(challenge.t, 'challenge');
  assert.equal(challenge.origin, origin);
  const auth = Buffer.concat([
    Buffer.from(`vibeke-relay/1 host-auth\0${origin}\0`),
    Buffer.from(challenge.nonce, 'base64url'),
  ]);
  control.ws.send(JSON.stringify({
    t: 'auth', host, pub: publicBytes.toString('base64url'),
    sig: sign(null, auth, privateKey).toString('base64url'),
  }));
  assert.equal((await control.json()).t, 'ok');
  // Host-signed ticket for this ephemeral host (subject `pid:check`, 5 minutes).
  const sub = 'pid:check';
  const exp = Math.floor(Date.now() / 1000) + 300;
  const ticketSig = sign(null, Buffer.from(`vibeke-relay/1 ticket\0${host}\0${sub}\0${String(exp)}`), privateKey);
  const ticket = `${Buffer.from(JSON.stringify({ v: 1, host, sub, exp })).toString('base64url')}.${ticketSig.toString('base64url')}`;
  const client = await connect(`/v1/connect?host=${host}&ticket=${encodeURIComponent(ticket)}`);
  const incoming = await control.json();
  assert.equal(incoming.t, 'incoming');
  const data = await connect('/v1/accept');
  const accept = Buffer.from(`vibeke-relay/1 accept\0${origin}\0${host}\0${incoming.generation}\0${incoming.conn}`);
  data.ws.send(JSON.stringify({
    t: 'accept', host, conn: incoming.conn, generation: incoming.generation,
    sig: sign(null, accept, privateKey).toString('base64url'),
  }));

  const hostPrivate = randomBytes(32);
  const devicePrivate = randomBytes(32);
  const prologue = Buffer.from(`vibeke-relay-smoke:${host}`);
  const device = new Initiator({ localPrivate: devicePrivate, remotePublic: x25519Public(hostPrivate), prologue });
  const gateway = new Responder({ localPrivate: hostPrivate, prologue });
  client.ws.send(new Uint8Array(device.writeFirst()));
  const first = gateway.readFirst(await data.binary());
  assert.deepEqual(first.remoteStatic, x25519Public(devicePrivate));
  const second = gateway.writeSecond();
  data.ws.send(new Uint8Array(second.message));
  const deviceSession = device.readSecond(await client.binary()).session;

  // Exercise chunked encrypted output as well as a small request in the other direction.
  const request = Buffer.from('relay deployment check');
  for (const frame of deviceSession.encrypt(request)) client.ws.send(new Uint8Array(frame));
  assert.deepEqual(Buffer.from(second.session.decrypt(await data.binary())!), request);
  const response = randomBytes(150_000);
  const frames = second.session.encrypt(response);
  for (const frame of frames) data.ws.send(new Uint8Array(frame));
  let received: Uint8Array | null = null;
  for (let i = 0; i < frames.length; i++) received = deviceSession.decrypt(await client.binary());
  assert.deepEqual(Buffer.from(received!), response);
  console.log(`PASS ${endpoint.origin}: host authentication, device routing, Noise IK, encrypted traffic both ways (150 KB response)`);
} finally {
  clearTimeout(deadline);
  for (const socket of sockets) socket.close();
}
