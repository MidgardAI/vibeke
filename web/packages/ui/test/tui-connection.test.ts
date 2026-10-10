import { expect, test } from 'bun:test';
import type { HostConnection, TuiCallbacks, TuiStream } from '@vibeke/core';
import { FakeClock, flush } from '../../core/test/helpers';
import { TuiConnection, type TuiState } from '../src/lib/tui-connection';
function harness(open?: (cb: TuiCallbacks) => Promise<TuiStream>, info = { scope: 'full', kind: 'device', features: ['wasm_tui'] } as Record<string, unknown>) {
  const clock = new FakeClock(); const states: TuiState[] = []; const notices: string[] = []; const crashes: unknown[] = [];
  let callback!: TuiCallbacks, changed!: () => void, status = 'online', calls = 0, resets = 0;
  const outgoing: Uint8Array[] = []; const sent: Uint8Array[] = []; const resolve: Array<() => void> = [];
  const stream: TuiStream = { id: 's', clientId: 'c', features: [], start() {}, close() {}, send(data) { sent.push(data); return new Promise<void>((r) => resolve.push(r)); } };
  const host = { getSnapshot: () => ({ status, info }), subscribe(fn: () => void) { changed = fn; return () => {}; }, reconnectNow() {}, async openTui(_protocol: number, cb: TuiCallbacks) { calls++; callback = cb; return open ? open(cb) : stream; } } as unknown as HostConnection;
  const runtime = { connected() {}, disconnected() { resets++; outgoing.length = 0; }, receive() {}, outgoing: () => outgoing.shift() ?? new Uint8Array() };
  const connection = new TuiConnection({ host, runtime, protocol: 7, clock, random: () => 0.5, state: (s) => states.push(s), wake() {}, notice: (n) => notices.push(n), crashed: (e) => crashes.push(e) });
  return { connection, runtime, crashes, stream, outgoing, sent, resolve, notices, states, clock, get calls() { return calls; }, get resets() { return resets; }, receive() { callback.frame(new Uint8Array()); }, closed() { callback.closed('lost'); }, status(s: string) { status = s; changed(); } };
}
test('buffered close during attach cannot overwrite reconnecting with Connected', async () => {
  const h = harness(async (cb) => ({ id: 's', clientId: 'c', features: [], start() { cb.closed('lost'); }, close() {}, async send() {} }));
  h.connection.start(); await flush(); expect(h.states.at(-1)?.kind).toBe('reconnecting');
  expect(h.states.some((s) => s.kind === 'connected')).toBe(false);
  h.connection.dispose();
});
test('transient attach retries, permanent incompatibility does not', async () => {
  const h = harness(async () => { throw new Error('unavailable'); }); h.connection.start(); await flush();
  expect(h.calls).toBe(1); await h.clock.advance(500); expect(h.calls).toBe(2);
  await h.clock.advance(1000); expect(h.calls).toBe(3); h.connection.dispose();
  const denied = harness(async () => { throw { kind: 'unsupported' }; }); denied.connection.start(); await flush();
  await denied.clock.advance(60_000); expect(denied.calls).toBe(1); expect(denied.states.at(-1)?.kind).toBe('blocked'); denied.connection.dispose();
});
test('slow transport has bounded concurrency and disconnect never replays queued input', async () => {
  const h = harness(); h.connection.start(); await flush();
  for (let n = 0; n < 10; n++) h.outgoing.push(Uint8Array.of(n));
  h.connection.flush(); expect(h.sent.map((b) => b[0])).toEqual([0, 1, 2, 3]);
  h.connection.flush(); expect(h.sent.length).toBe(4);
  h.closed(); expect(h.outgoing).toEqual([]); expect(h.notices.length).toBe(1);
  for (const resolve of h.resolve) resolve(); await flush();
  await h.clock.advance(500); h.connection.flush(); expect(h.sent.length).toBe(4); expect(h.states.at(-1)?.kind).toBe('connected');
  h.status('revoked'); expect(h.states.at(-1)?.kind).toBe('blocked'); await h.clock.advance(60_000); expect(h.calls).toBe(2); h.connection.dispose();
});
test('late attach completion from an old generation is closed without replacing the current stream', async () => {
  const pending: Array<(stream: TuiStream) => void> = [];
  const h = harness(() => new Promise<TuiStream>((resolve) => pending.push(resolve)));
  h.connection.start(); await flush(); h.connection.reconnect(); await flush();
  let oldClosed = 0;
  pending[0]!({ ...h.stream, close() { oldClosed++; } }); await flush();
  expect(oldClosed).toBe(1); expect(h.states.some((s) => s.kind === 'connected')).toBe(false);
  pending[1]!(h.stream); await flush(); expect(h.states.at(-1)?.kind).toBe('connected');
  h.connection.dispose();
});

test('WASM traps stop the transport without calling the damaged runtime again', async () => {
  for (const operation of ['receive', 'outgoing', 'connected', 'disconnected'] as const) {
    const h = harness();
    h.runtime[operation] = () => { throw new WebAssembly.RuntimeError('unreachable'); };
    h.connection.start(); await flush();
    if (operation === 'receive') h.receive();
    if (operation === 'outgoing') h.connection.flush();
    if (operation === 'disconnected') h.closed();
    expect(h.crashes.length).toBe(1);
    const calls = h.calls;
    h.connection.reconnect(); await h.clock.advance(60_000);
    expect(h.calls).toBe(calls); expect(h.crashes.length).toBe(1);
    h.connection.dispose();
  }
});
test('host status changes preserve the reason a terminal is blocked', async () => {
  const h = harness(); h.connection.start(); await flush();
  h.connection.fail('Input was discarded');
  h.status('offline'); h.status('online');
  expect(h.states.at(-1)).toEqual({ kind: 'blocked', message: 'Input was discarded' });
  expect(h.calls).toBe(1); h.connection.dispose();
});

test('scoped shares attach only to hosts that advertise the scoped terminal boundary', async () => {
  for (const scope of ['view', 'approve', 'full']) {
    const h = harness(undefined, { scope, kind: 'share', limit: { pane: 'p1' }, features: ['wasm_tui', 'wasm_tui_share'] });
    h.connection.start(); await flush(); expect(h.calls).toBe(1); expect(h.states.at(-1)?.kind).toBe('connected'); h.connection.dispose();
  }
  for (const info of [
    { scope: 'view', kind: 'share', limit: { pane: 'p1' }, features: ['wasm_tui'] },
    { scope: 'full', kind: 'peer', features: ['wasm_tui', 'wasm_tui_share'] },
    { scope: 'full', kind: 'share', features: ['wasm_tui', 'wasm_tui_share'] },
  ]) {
    const h = harness(undefined, info); h.connection.start(); await flush(); expect(h.calls).toBe(0); expect(h.states.at(-1)?.kind).toBe('blocked'); h.connection.dispose();
  }
});
