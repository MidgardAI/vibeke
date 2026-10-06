import { describe, expect, test } from 'bun:test';
import { randomBytes } from '@noble/hashes/utils.js';
import { ChannelError, type MessageChannel } from '../src/channel';
import { OutcomeUnknownError, RpcClient, RpcError, uuidv4 } from '../src/rpc';
import { FakeClock, flush } from './helpers';

class FakeChannel implements MessageChannel {
  sent: any[] = [];
  closed = false;
  closeErr: ChannelError | null = null;
  private msg = new Set<(m: unknown) => void>();
  private cls = new Set<(e: ChannelError | null) => void>();
  send(m: unknown) {
    if (this.closed) throw new ChannelError('closed', 'closed');
    this.sent.push(m);
  }
  onMessage(cb: (m: unknown) => void) {
    this.msg.add(cb);
    return () => this.msg.delete(cb);
  }
  onClose(cb: (e: ChannelError | null) => void) {
    this.cls.add(cb);
    return () => this.cls.delete(cb);
  }
  close(e?: ChannelError) {
    if (this.closed) return;
    this.closed = true;
    this.closeErr = e ?? null;
    this.cls.forEach((cb) => cb(e ?? null));
  }
  deliver(m: unknown) {
    this.msg.forEach((cb) => cb(m));
  }
}

function setup(keepalive = false) {
  const clock = new FakeClock();
  const ch = new FakeChannel();
  const rpc = new RpcClient(ch, { clock, random: randomBytes, keepalive });
  return { clock, ch, rpc };
}

describe('rpc', () => {
  test('matches responses by id, out of order', async () => {
    const { ch, rpc } = setup();
    const a = rpc.request('pane.read', { pane: 'p1' });
    const b = rpc.request('pane.read', { pane: 'p2' });
    expect(ch.sent.map((m) => [m.jsonrpc, m.id, m.method])).toEqual([
      ['2.0', 1, 'pane.read'],
      ['2.0', 2, 'pane.read'],
    ]);
    ch.deliver({ jsonrpc: '2.0', id: 2, result: { text: 'two' } });
    ch.deliver({ jsonrpc: '2.0', id: 99, result: 'stray' }); // ignored
    ch.deliver({ jsonrpc: '2.0', id: 1, result: { text: 'one' } });
    expect(await a).toEqual({ text: 'one' });
    expect(await b).toEqual({ text: 'two' });
  });

  test('errors carry data.kind', async () => {
    const { ch, rpc } = setup();
    const p = rpc.request('interaction.answer', {}, { mutating: true });
    ch.deliver({ jsonrpc: '2.0', id: 1, error: { code: -32004, message: 'stale', data: { kind: 'stale' } } });
    const e: any = await p.catch((x: any) => x);
    expect(e).toBeInstanceOf(RpcError);
    expect(e.kind).toBe('stale');
    expect(e.code).toBe(-32004);
  });

  test('mutating calls get an op_id; caller op_id is kept', () => {
    const { ch, rpc } = setup();
    void rpc.request('pane.send_text', { pane: 'p', text: 'x' }, { mutating: true });
    void rpc.request('pane.send_text', { pane: 'p', text: 'x', op_id: 'mine' }, { mutating: true });
    void rpc.request('pane.read', { pane: 'p' });
    expect(ch.sent[0].params.op_id).toMatch(/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/);
    expect(ch.sent[1].params.op_id).toBe('mine');
    expect(ch.sent[2].params.op_id).toBeUndefined();
    expect(uuidv4(randomBytes)).not.toBe(uuidv4(randomBytes));
  });

  test('timeout → OutcomeUnknownError, late response ignored, no retry', async () => {
    const { clock, ch, rpc } = setup();
    const p = rpc.request('agent.prompt', { target: 'a', text: 'hi' }, { mutating: true, timeoutMs: 5000 });
    const caught = p.catch((x: any) => x);
    await clock.advance(5000);
    const e: any = await caught;
    expect(e).toBeInstanceOf(OutcomeUnknownError);
    expect(e.reason).toBe('timeout');
    expect(e.mutating).toBe(true);
    expect(e.opId).toBe(ch.sent[0].params.op_id);
    ch.deliver({ jsonrpc: '2.0', id: 1, result: {} });
    expect(ch.sent.length).toBe(1); // never re-sent
  });

  test('close with requests in flight → OutcomeUnknownError', async () => {
    const { ch, rpc } = setup();
    const p = rpc.request('pane.send_keys', { pane: 'p', keys: ['Enter'] }, { mutating: true }).catch((x: any) => x);
    ch.close(new ChannelError('decrypt', 'bad frame'));
    const e: any = await p;
    expect(e).toBeInstanceOf(OutcomeUnknownError);
    expect(e.reason).toBe('closed');
    expect(await rpc.request('ping').catch((x: any) => x)).toBeInstanceOf(OutcomeUnknownError);
  });

  test('notifications dispatch by method and wildcard', () => {
    const { ch, rpc } = setup();
    const got: string[] = [];
    rpc.on('event', (p: any) => got.push(`event:${p.seq}`));
    const off = rpc.on('*', (_p, m) => got.push(`*:${m}`));
    ch.deliver({ jsonrpc: '2.0', method: 'event', params: { seq: 1 } });
    off();
    ch.deliver({ jsonrpc: '2.0', method: 'events.reset', params: {} });
    expect(got).toEqual(['event:1', '*:event']);
  });

  test('host→device requests get method-not-found', () => {
    const { ch } = setup();
    ch.deliver({ jsonrpc: '2.0', id: 7, method: 'x', params: {} });
    expect(ch.sent[0]).toEqual({ jsonrpc: '2.0', id: 7, error: { code: -32601, message: 'method not found' } });
  });

  test('pings every 20 s and closes after 60 s of silence', async () => {
    const { clock, ch } = setup(true);
    await clock.advance(20_000);
    expect(ch.sent.map((m) => m.method)).toEqual(['ping']);
    ch.deliver({ jsonrpc: '2.0', id: ch.sent[0].id, result: {} }); // t=20s: alive
    await clock.advance(40_000); // t=60s: last receipt 40 s ago
    expect(ch.closed).toBe(false);
    expect(ch.sent.filter((m) => m.method === 'ping').length).toBe(3);
    await clock.advance(20_000); // t=80s: 60 s silence
    await flush();
    expect(ch.closed).toBe(true);
    expect(ch.closeErr?.code).toBe('timeout');
    expect(clock.pending).toBe(0); // all timers cleaned up
  });
});
