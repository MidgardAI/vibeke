import { describe, expect, test } from 'bun:test';
import { Channel, ChannelError } from '../src/channel';
import { helloDevice, helloPair } from '../src/hello';
import { x25519Public } from '../src/keys';
import { RpcClient } from '../src/rpc';
import { FakeClock, flush, serveGateway, socketPair, type GatewayCtx, type MockGatewayOptions } from './helpers';

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s * 31 + i * 7) & 0xff);
const HOST = seeded(1);
const DEV = seeded(2);

function open(gw: Partial<MockGatewayOptions> = {}, o: { psk?: Uint8Array; pid?: string; hostKey?: Uint8Array } = {}) {
  const clock = new FakeClock();
  const [client, server] = socketPair();
  let ctx: GatewayCtx | null = null;
  serveGateway(server, {
    hostPrivate: HOST,
    handle: (m, p) => (m === 'echo' ? p : {}),
    ...gw,
    onReady: (c) => {
      ctx = c;
      gw.onReady?.(c);
    },
  });
  const p = Channel.connect({
    socket: client,
    hello: o.pid ? helloPair(o.pid) : helloDevice(),
    hostKey: o.hostKey ?? x25519Public(HOST),
    devicePrivate: DEV,
    psk: o.psk,
    clock,
  });
  return { clock, client, server, p, ctx: () => ctx! };
}

describe('channel (TS initiator ↔ TS responder)', () => {
  test('handshake, host info, request/response incl. chunked messages', async () => {
    const { p, ctx, client, clock } = open();
    const ch = await p;
    expect(ch.host.host_name).toBe('devbox');
    expect(ctx().deviceKey).toEqual(x25519Public(DEV));
    expect(ctx().hello).toBe('{"v":1,"proto":"vibeke-e2e/1","mode":"device"}');
    expect(typeof client.sent[0]).toBe('string');
    expect(client.sent.slice(1).every((m) => m instanceof Uint8Array)).toBe(true);

    const rpc = new RpcClient(ch, { clock, random: (n) => new Uint8Array(n), keepalive: false });
    const blob = 'x'.repeat(300_000); // 5 chunks each way
    const res = await rpc.request<{ blob: string }>('echo', { blob });
    expect(res.blob).toBe(blob);
    expect(client.sent.length).toBe(2 + 5);
  });

  test('pairing mode uses IKpsk2', async () => {
    const psk = seeded(9);
    const ch = await open({ psks: { 'pid-1': psk } }, { pid: 'pid-1', psk }).p;
    expect(ch.host.host_name).toBe('devbox');
  });

  test('wrong psk fails the handshake', async () => {
    const e = await open({ psks: { 'pid-1': seeded(9) } }, { pid: 'pid-1', psk: seeded(8) }).p.catch((x: any) => x);
    expect(e).toBeInstanceOf(ChannelError);
    expect(e.code).toBe('handshake');
  });

  test('wrong pinned host key → gateway drops connection', async () => {
    const e = await open({}, { hostKey: x25519Public(seeded(5)) }).p.catch((x: any) => x);
    expect(e).toBeInstanceOf(ChannelError);
    expect(e.code).toBe('closed');
  });

  test('plaintext unauthorized surfaces as a typed, unauthenticated error', async () => {
    const e = await open({ authorize: () => false }).p.catch((x: any) => x);
    expect(e.code).toBe('unauthorized');
    expect(e.authenticated).toBe(false);
  });

  test('unsupported_version', async () => {
    const [client, server] = socketPair();
    server.onmessage = () => {
      server.send('{"error":"unsupported_version","supported":[2]}');
      server.close(4000);
    };
    const e = await Channel.connect({
      socket: client,
      hello: helloDevice(),
      hostKey: x25519Public(HOST),
      devicePrivate: DEV,
      clock: new FakeClock(),
    }).catch((x: any) => x);
    expect(e.code).toBe('unsupported_version');
    expect(e.opts.remote.supported).toEqual([2]);
  });

  test('relay close before handshake keeps the close code', async () => {
    const [client, server] = socketPair();
    server.onmessage = () => server.close(4404, 'host_offline');
    const e = await Channel.connect({
      socket: client,
      hello: helloDevice(),
      hostKey: x25519Public(HOST),
      devicePrivate: DEV,
      clock: new FakeClock(),
    }).catch((x: any) => x);
    expect(e.code).toBe('closed');
    expect(e.closeCode).toBe(4404);
    expect(e.message).toContain('host_offline');
  });

  test('handshake timeout', async () => {
    const clock = new FakeClock();
    const [client, server] = socketPair();
    server.onmessage = () => {}; // never answers
    const p = Channel.connect({ socket: client, hello: helloDevice(), hostKey: x25519Public(HOST), devicePrivate: DEV, clock, timeoutMs: 1000 }).catch(
      (x: any) => x,
    );
    await clock.advance(1000);
    expect((await p).code).toBe('timeout');
    expect(client.state).toBe('closed');
  });

  test('tampered transport frame closes the channel', async () => {
    const { p, ctx } = open();
    const ch = await p;
    const closed = new Promise<ChannelError | null>((r) => ch.onClose(r));
    const [f] = ctx().session.encrypt(new TextEncoder().encode('{"jsonrpc":"2.0","method":"x"}'));
    f![3]! ^= 0xff;
    ctx().socket.send(f!);
    const err = await closed;
    expect(err?.code).toBe('decrypt');
    expect(ch.closed).toBe(true);
  });

  test('text message after handshake is a protocol error', async () => {
    const { p, ctx } = open();
    const ch = await p;
    const closed = new Promise<ChannelError | null>((r) => ch.onClose(r));
    ctx().socket.send('{"error":"unauthorized"}');
    expect((await closed)?.code).toBe('protocol');
  });

  test('messages before the first listener are buffered; async iterator ends on close', async () => {
    const { p, ctx } = open({ onReady: (c) => c.notify('hello.early', { n: 1 }) });
    const ch = await p;
    await flush();
    const got: unknown[] = [];
    const it = (async () => {
      for await (const m of ch.messages()) got.push(m);
    })();
    ctx().notify('later', { n: 2 });
    await flush();
    ch.close();
    await it;
    expect(got).toEqual([
      { jsonrpc: '2.0', method: 'hello.early', params: { n: 1 } },
      { jsonrpc: '2.0', method: 'later', params: { n: 2 } },
    ]);
  });

  test('stalled partial message closes after 30 s', async () => {
    const { p, ctx, clock } = open();
    const ch = await p;
    const closed = new Promise<ChannelError | null>((r) => ch.onClose(r));
    const frames = ctx().session.encrypt(new Uint8Array(70_000).fill(32));
    ctx().socket.send(frames[0]!); // only the first chunk
    await flush();
    await clock.advance(30_002);
    expect((await closed)?.code).toBe('timeout');
  });
});
