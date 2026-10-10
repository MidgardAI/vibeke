import { expect, test } from 'bun:test';
import { b64, openTuiStream, RpcClient, type MessageChannel, type ChannelError } from '../src';
import { FakeClock } from './helpers';

function setup() {
  const sent: Array<{ id: number; method: string; params: Record<string, unknown> }> = [];
  const messages = new Set<(v: unknown) => void>();
  const closes = new Set<(e: ChannelError | null) => void>();
  const channel: MessageChannel = {
    closed: false,
    send(v) { sent.push(v as (typeof sent)[number]); },
    onMessage(cb) { messages.add(cb); return () => messages.delete(cb); },
    onClose(cb) { closes.add(cb); return () => closes.delete(cb); },
    close() { closes.forEach((cb) => cb(null)); },
  };
  const rpc = new RpcClient(channel, { clock: new FakeClock(), random: (n) => new Uint8Array(n), keepalive: false });
  const deliver = (v: unknown) => messages.forEach((cb) => cb(v));
  const attach = () => deliver({ id: sent.find((s) => s.method === 'tui.attach')!.id, result: { stream: 's1', client_id: 'c1', protocol: 9, features: [] } });
  const frame = (stream: string, data: number[]) => deliver({ method: 'tui.frame', params: { stream, data: b64.encode(new Uint8Array(data)) } });
  return { rpc, sent, deliver, attach, frame };
}

test('buffers frames before attach completes and delivers only this stream after start', async () => {
  const h = setup(); const received: number[][] = [];
  const opening = openTuiStream(h.rpc, 9, { frame: (b) => received.push([...b]), closed: () => {} });
  h.frame('s1', [1,2]); h.frame('old', [9]); h.attach();
  const stream = await opening;
  expect(received).toEqual([]);
  stream.start(); h.frame('s1', [3]); h.frame('old', [8]);
  expect(received).toEqual([[1,2],[3]]);
  stream.close(); h.frame('s1', [4]);
  expect(received).toEqual([[1,2],[3]]);
  h.rpc.close();
});

test('cancelled attach detaches a late server stream', async () => {
  const h = setup(); const controller = new AbortController();
  const opening = openTuiStream(h.rpc, 9, { frame: () => {}, closed: () => {} }, controller.signal).catch((e) => e);
  controller.abort(); h.attach();
  expect(await opening).toBeInstanceOf(Error);
  expect(h.sent.some((s) => s.method === 'tui.detach' && s.params.stream === 's1')).toBe(true);
  h.rpc.close();
});

test('input is acknowledged once and is never replayed after a stream closes', async () => {
  const h = setup(); const reasons: string[] = [];
  const opening = openTuiStream(h.rpc, 9, { frame: () => {}, closed: (r) => reasons.push(r) });
  h.attach(); const stream = await opening; stream.start();
  const sent = stream.send(new Uint8Array([1,2,3]));
  const call = h.sent.find((s) => s.method === 'tui.send')!;
  h.deliver({ id: call.id, result: {} }); await sent;
  h.deliver({ method: 'tui.closed', params: { stream: 's1', reason: 'revoked' } });
  expect(reasons).toEqual(['revoked']);
  await expect(stream.send(new Uint8Array([4]))).rejects.toThrow('disconnected');
  expect(h.sent.filter((s) => s.method === 'tui.send')).toHaveLength(1);
  h.rpc.close();
});
