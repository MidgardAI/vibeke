import { describe, expect, test } from 'bun:test';
import * as b64 from '../src/b64';
import { HostManager, NotConnectedError, recordFromHello, type HostRecord, type HostStore } from '../src/hosts';
import { x25519Public } from '../src/keys';
import type { AppEvent } from '../src/model';
import { flush, serveGateway, testPlatform, type GatewayCtx, type MemSocket } from './helpers';

const seeded = (s: number) => Uint8Array.from({ length: 32 }, (_, i) => (s * 11 + i * 3) & 0xff);
const HOST = seeded(1);
const DEV = seeded(2);

const record: HostRecord = {
  host_id: 'h1',
  relay: 'wss://relay.example',
  hk: b64.encode(x25519Public(HOST)),
  device_id: 'd1',
  name: 'devbox',
  scope: 'full',
};

const dashboard = (at: number) => ({
  at,
  session: 'main',
  machine: 'devbox',
  workspaces: [],
  tabs: [],
  panes: [],
  runs: [],
  interactions: [{ id: 'i1', kind: 'Approval', status: 'Open', delivery: 'None', action: null, answer: null }],
  tasks: [],
  notifications_unread: 0,
});

/** A scriptable gateway: records calls; `mode` decides how the next connection behaves. */
function harness(rec: HostRecord = record) {
  const calls: [string, any][] = [];
  const state = {
    at: 10,
    resetOnResume: false,
    refuse: false as boolean | 'offline',
    ctx: null as GatewayCtx | null,
    sockets: [] as MemSocket[],
    hello: {} as Record<string, unknown>,
    puts: [] as HostRecord[],
    /** While true, `dashboard.get` answers only when its gate is released (in any order). */
    gated: false,
    gates: [] as { at: number; release(): void; fail(): void }[],
  };
  const platform = testPlatform((sock) => {
    if (state.refuse === 'offline') return false;
    state.sockets.push(sock);
    serveGateway(sock, {
      hostPrivate: HOST,
      authorize: () => !state.refuse,
      onReady: (c) => (state.ctx = c),
      handle(method, params) {
        calls.push([method, params]);
        switch (method) {
          case 'hello':
            return { host_name: 'devbox', device_id: 'd1', scope: 'full', server_version: '0.1.0', features: [], ...state.hello };
          case 'dashboard.get': {
            const d = dashboard(state.at);
            if (!state.gated) return d;
            return new Promise((resolve, reject) => state.gates.push({ at: d.at, release: () => resolve(d), fail: () => reject({ code: -32000, message: 'busy' }) }));
          }
          case 'events.subscribe':
            return params.after !== undefined && state.resetOnResume && params.after !== state.at ? { at: state.at, reset: true } : { at: state.at };
          default:
            return {};
        }
      },
    });
  });
  const store: HostStore = { list: async () => [rec], put: async (r) => void state.puts.push(r), remove: async () => {} };
  const mgr = new HostManager({ platform, store, devicePrivate: DEV, client: { client: 'test', version: '0' } });
  const h = () => mgr.get('h1')!;
  const methods = () => calls.map((c) => c[0]);
  return { platform, mgr, h, calls, methods, state };
}

describe('HostManager', () => {
  test('connects, loads dashboard (normalized), subscribes at the barrier', async () => {
    const { mgr, h, calls, platform } = harness();
    const seen: string[] = [];
    mgr.subscribe(() => seen.push(h()?.getSnapshot().status));
    await mgr.start();
    await flush(20);
    const s = h().getSnapshot();
    expect(s.status).toBe('online');
    expect(s.info?.scope).toBe('full');
    expect(s.cursor).toBe(10);
    expect(s.dashboard?.interactions[0]).toMatchObject({ kind: 'approval', status: 'open', delivery: 'none' });
    expect(calls.map((c) => c[0])).toEqual(['hello', 'dashboard.get', 'events.subscribe']);
    expect(calls[0]![1]).toEqual({ client: 'test', version: '0', visible: true });
    expect(calls[2]![1]).toEqual({ after: 10 });
    expect(seen).toContain('connecting');
    expect(mgr.getSnapshot()).toBe(mgr.getSnapshot()); // stable for useSyncExternalStore
    expect(platform.urls[0]).toBe('wss://relay.example/v1/connect?host=h1');
    mgr.stop();
  });

  test('events advance the cursor, dedupe by seq, and trigger a debounced refetch', async () => {
    const { mgr, h, methods, state, platform } = harness();
    await mgr.start();
    await flush(20);
    const got: AppEvent[] = [];
    h().onEvent((e) => got.push(e));
    const ev = (seq: number, type = 'interaction.opened') => state.ctx!.notify('event', { seq, ts: 0, type, subject: {}, data: {} });
    ev(11);
    ev(11); // duplicate
    ev(12, 'agent.state_changed');
    await flush();
    expect(got.map((e) => e.seq)).toEqual([11, 12]);
    expect(h().getSnapshot().cursor).toBe(12);
    await platform.clock.advance(300);
    expect(methods().filter((m) => m === 'dashboard.get').length).toBe(2); // one coalesced refetch
    mgr.stop();
  });

  test('subscribeEvents: per-host listeners (and onAnyEvent) see deduped events; unsubscribe stops them', async () => {
    const { mgr, state } = harness();
    const got: string[] = [];
    const any: string[] = [];
    // Subscribing before the connection exists works (the UI subscribes on mount).
    const off = mgr.subscribeEvents('h1', (e) => got.push(`${e.seq}:${e.type}`));
    mgr.subscribeEvents('other', () => got.push('wrong host'));
    const offAny = mgr.onAnyEvent((id, e) => any.push(`${id}:${e.seq}`));
    await mgr.start();
    await flush(20);
    const ev = (seq: number, type: string) => state.ctx!.notify('event', { seq, ts: 0, type, subject: { run: 'r1' }, data: {} });
    ev(11, 'agent.turn_started');
    ev(11, 'agent.turn_started');
    ev(12, 'agent.turn_completed');
    await flush();
    expect(got).toEqual(['11:agent.turn_started', '12:agent.turn_completed']);
    expect(any).toEqual(['h1:11', 'h1:12']);
    off();
    offAny();
    ev(13, 'agent.usage');
    await flush();
    expect(got.length).toBe(2);
    expect(any.length).toBe(2);
    mgr.stop();
  });

  test('reconnects with backoff and resumes from the cursor; reset falls back to dashboard', async () => {
    const { mgr, h, calls, state, platform } = harness();
    await mgr.start();
    await flush(20);
    state.ctx!.notify('event', { seq: 15, ts: 0, type: 'pane.created', subject: {}, data: {} });
    await flush();
    await platform.clock.advance(300); // debounced refetch lands: the dashboard is fresh
    calls.length = 0;

    state.ctx!.close(1006);
    await flush();
    const s = h().getSnapshot();
    expect(s.status).toBe('offline');
    expect(s.nextRetryAt! - platform.clock.now()).toBeGreaterThanOrEqual(500);
    expect(s.nextRetryAt! - platform.clock.now()).toBeLessThan(1000);
    await platform.clock.advance(1000);
    await flush(20);
    expect(h().getSnapshot().status).toBe('online');
    // Resume: no snapshot needed, subscription continues after the last seen event.
    expect(calls.map((c) => c[0])).toEqual(['hello', 'events.subscribe']);
    expect(calls[1]![1]).toEqual({ after: 15 });

    // Gateway ring no longer has our cursor → {reset:true} → snapshot + subscribe at the barrier.
    calls.length = 0;
    state.at = 40;
    state.resetOnResume = true;
    state.ctx!.close(1006);
    await platform.clock.advance(1000);
    await flush(20);
    expect(calls.map((c) => [c[0], c[1]])).toEqual([
      ['hello', expect.anything()],
      ['events.subscribe', { after: 15 }],
      ['dashboard.get', {}],
      ['events.subscribe', { after: 40 }],
    ]);
    expect(h().getSnapshot().cursor).toBe(40);
    mgr.stop();
  });

  test('reconnect refetches a dashboard left stale by a cancelled debounced refresh', async () => {
    const { mgr, h, calls, state, platform } = harness();
    state.at = 1;
    await mgr.start();
    await flush(20);
    expect(h().getSnapshot().dashboard?.at).toBe(1);
    // An event advances the cursor; the debounced refetch is pending when the link drops.
    state.ctx!.notify('event', { seq: 2, ts: 0, type: 'interaction.opened', subject: {}, data: {} });
    await flush();
    expect(h().getSnapshot().cursor).toBe(2);
    state.ctx!.close(1006);
    await flush();
    expect(h().getSnapshot().status).toBe('offline');
    calls.length = 0;
    state.at = 2;
    await platform.clock.advance(1000);
    await flush(20);
    const s = h().getSnapshot();
    expect(s.status).toBe('online');
    expect(calls.map((c) => c[0])).toEqual(['hello', 'events.subscribe', 'dashboard.get']);
    expect(calls[1]![1]).toEqual({ after: 2 });
    expect(s.dashboard?.at).toBe(2);
    // Once fresh, a later clean reconnect does not refetch again.
    calls.length = 0;
    state.ctx!.close(1006);
    await platform.clock.advance(1000);
    await flush(20);
    expect(calls.map((c) => c[0])).toEqual(['hello', 'events.subscribe']);
    mgr.stop();
  });

  test('overlapping refreshes are coalesced and never regress the dashboard (20 → 10)', async () => {
    const { mgr, h, methods, state, platform } = harness();
    await mgr.start();
    await flush(20);
    const seen: number[] = [];
    h().subscribe(() => seen.push(h().getSnapshot().dashboard?.at ?? -1));
    state.gated = true;
    // A refresh is in flight (snapshot at 10) when events arrive and more refreshes are asked for.
    const first = h().refresh();
    await flush();
    expect(state.gates.length).toBe(1);
    state.at = 20;
    state.ctx!.notify('event', { seq: 11, ts: 0, type: 'interaction.opened', subject: {}, data: {} });
    await flush();
    await platform.clock.advance(300); // the debounced refresh joins the one in flight
    const second = h().refresh();
    await flush();
    expect(state.gates.length).toBe(1); // still one request on the wire
    state.gates.shift()!.release();
    await flush(10);
    // The follow-up fetch (issued after the events) runs once and lands at 20.
    expect(state.gates.length).toBe(1);
    expect(state.gates[0]!.at).toBe(20);
    state.gates.shift()!.release();
    await Promise.all([first, second]);
    expect(h().getSnapshot().dashboard?.at).toBe(20);
    expect(methods().filter((m) => m === 'dashboard.get').length).toBe(3); // connect + 2
    for (let i = 1; i < seen.length; i++) expect(seen[i]!).toBeGreaterThanOrEqual(seen[i - 1]!);
    mgr.stop();
  });

  test('a failed refresh does not lose the queued follow-up (issued after a backoff)', async () => {
    const { mgr, h, methods, state, platform } = harness();
    await mgr.start();
    await flush(20);
    state.gated = true;
    const first = h().refresh();
    await flush();
    state.at = 20;
    const second = h().refresh(); // joins: wants a fetch issued after now
    await flush();
    expect(state.gates.length).toBe(1);
    const outcomes: string[] = [];
    first.then(() => outcomes.push('first ok'), () => outcomes.push('first err'));
    second.then(() => outcomes.push('second ok'), () => outcomes.push('second err'));
    state.gates.shift()!.fail();
    await flush(10);
    expect(outcomes).toEqual([]); // not rejected: the follow-up is still owed
    await platform.clock.advance(250);
    await flush(10);
    expect(state.gates.length).toBe(1);
    expect(state.gates[0]!.at).toBe(20);
    state.gates.shift()!.release();
    await flush(10);
    expect(outcomes.sort()).toEqual(['first ok', 'second ok']);
    expect(h().getSnapshot().dashboard?.at).toBe(20);
    expect(methods().filter((m) => m === 'dashboard.get').length).toBe(3); // connect + 2
    mgr.stop();
  });

  test('when the follow-up fails too, all waiters reject together', async () => {
    const { mgr, h, state, platform } = harness();
    await mgr.start();
    await flush(20);
    state.gated = true;
    const first = h().refresh();
    await flush();
    const second = h().refresh();
    const outcomes: string[] = [];
    first.then(() => outcomes.push('ok'), () => outcomes.push('err'));
    second.then(() => outcomes.push('ok'), () => outcomes.push('err'));
    await flush();
    state.gates.shift()!.fail();
    await platform.clock.advance(250);
    await flush(10);
    state.gates.shift()!.fail();
    await flush(10);
    expect(outcomes).toEqual(['err', 'err']);
    // The next refresh starts afresh.
    state.gated = false;
    await h().refresh();
    mgr.stop();
  });

  test('an older dashboard response arriving last is ignored', async () => {
    const { mgr, h, state } = harness();
    await mgr.start();
    await flush(20);
    state.gated = true;
    // A refresh (at 10) is in flight when events.reset resyncs the same connection (at 20).
    void h().refresh();
    await flush();
    state.at = 20;
    state.ctx!.notify('events.reset', {});
    await flush();
    expect(state.gates.map((g) => g.at)).toEqual([10, 20]);
    state.gates[1]!.release(); // the newer response first
    await flush(10);
    expect(h().getSnapshot().dashboard?.at).toBe(20);
    state.gates[0]!.release(); // then the older one
    await flush(10);
    expect(h().getSnapshot().dashboard?.at).toBe(20);
    expect(h().getSnapshot().cursor).toBe(20);
    mgr.stop();
  });

  test('events.reset notification refetches the dashboard', async () => {
    const { mgr, h, methods, state } = harness();
    await mgr.start();
    await flush(20);
    state.at = 77;
    state.ctx!.notify('events.reset', {});
    await flush(20);
    expect(methods().slice(-2)).toEqual(['dashboard.get', 'events.subscribe']);
    expect(h().getSnapshot().cursor).toBe(77);
    mgr.stop();
  });

  test('backoff grows to 30 s; visibility reconnects immediately', async () => {
    const { mgr, h, state, platform } = harness();
    state.refuse = 'offline';
    await mgr.start();
    await flush(10);
    const delays: number[] = [];
    for (let i = 0; i < 7; i++) {
      const s = h().getSnapshot();
      expect(s.status).toBe('offline');
      expect(s.closeCode).toBe(4404);
      delays.push(s.nextRetryAt! - platform.clock.now());
      await platform.clock.advance(s.nextRetryAt! - platform.clock.now());
      await flush(10);
    }
    const bases = [1, 2, 4, 8, 16, 30, 30].map((x) => x * 1000);
    delays.forEach((d, i) => {
      expect(d).toBeGreaterThanOrEqual(bases[i]! / 2);
      expect(d).toBeLessThan(bases[i]!);
    });
    state.refuse = false;
    const before = platform.urls.length;
    platform.lifecycle.show();
    await flush(20);
    expect(platform.urls.length).toBe(before + 1);
    expect(h().getSnapshot().status).toBe('online');
    mgr.stop();
  });

  test('three consecutive unauthorized closes → revoked, no more retries', async () => {
    const { mgr, h, state, platform } = harness();
    state.refuse = true;
    await mgr.start();
    await flush(10);
    expect(h().getSnapshot().status).toBe('unauthorized');
    await platform.clock.advance(1000);
    expect(h().getSnapshot().status).toBe('unauthorized');
    await platform.clock.advance(2000);
    expect(h().getSnapshot().status).toBe('revoked');
    const n = platform.urls.length;
    await platform.clock.advance(60_000);
    expect(platform.urls.length).toBe(n);
    mgr.stop();
  });

  test('authenticated device.revoked → revoked immediately', async () => {
    const { mgr, h, state, platform } = harness();
    await mgr.start();
    await flush(20);
    state.ctx!.notify('device.revoked', {});
    await flush(10);
    expect(h().getSnapshot().status).toBe('revoked');
    const n = platform.urls.length;
    await platform.clock.advance(60_000);
    expect(platform.urls.length).toBe(n);
  });

  test('typed requests: op_id on mutations, NotConnectedError when offline', async () => {
    const { mgr, h, calls } = harness();
    expect(mgr.get('h1')).toBeUndefined();
    await mgr.start();
    const offline = await h().request('ping', {}).catch((x: any) => x);
    expect(offline).toBeInstanceOf(NotConnectedError);
    await flush(20);
    await h().request('pane.send_text', { pane: 'p1', text: 'ls', submit: true });
    await h().request('pane.read', { pane: 'p1' });
    const send = calls.find((c) => c[0] === 'pane.send_text')![1];
    expect(typeof send.op_id).toBe('string');
    expect(calls.find((c) => c[0] === 'pane.read')![1].op_id).toBeUndefined();
    mgr.stop();
  });

  test('visibility changes are reported to online hosts', async () => {
    const { mgr, calls, platform } = harness();
    await mgr.start();
    await flush(20);
    platform.lifecycle.hide();
    await flush(10);
    expect(calls.at(-1)).toEqual(['client.visibility', { visible: false }]);
    mgr.stop();
  });

  test('share hosts go expired at `until` and are not reconnected', async () => {
    const { mgr, h, platform } = harness({ ...record, kind: 'share', until: 1_800_000_010 }); // FakeClock starts at 1_800_000_000_000
    await mgr.start();
    await flush(20);
    expect(h().getSnapshot().status).toBe('online');
    await platform.clock.advance(10_000);
    await flush(10);
    expect(h().getSnapshot().status).toBe('expired');
    expect(h().getSnapshot().dashboard).toBeNull();
    const n = platform.urls.length;
    platform.lifecycle.show();
    await platform.clock.advance(60_000);
    expect(platform.urls.length).toBe(n);
    mgr.stop();
  });

  test('an already-expired share host never connects', async () => {
    const { mgr, h, platform } = harness({ ...record, kind: 'share', until: 1_700_000_000 });
    await mgr.start();
    await flush(10);
    expect(h().getSnapshot().status).toBe('expired');
    expect(platform.urls.length).toBe(0);
    mgr.stop();
  });
});

describe('hello describes this device', () => {
  test('recordFromHello takes kind, expiry and limit from the host', () => {
    expect(recordFromHello(record, { kind: 'device', expires_at: null, limit: null })).toBeNull();
    expect(recordFromHello(record, {})).toBeNull(); // older gateway: keep what the link said
    const share = recordFromHello(record, { kind: 'share', expires_at: 2_000_000_000, limit: { workspace: null, pane: 'p1' } })!;
    expect(share).toMatchObject({ kind: 'share', until: 2_000_000_000, limit: { pane: 'p1' } });
    expect(share.limit).toEqual({ pane: 'p1' });
    expect(recordFromHello(share, { kind: 'share', expires_at: 2_000_000_000, limit: { pane: 'p1' } })).toBeNull();
    const extended = recordFromHello(share, { kind: 'share', expires_at: 2_100_000_000, limit: { pane: 'p1' } })!;
    expect(extended.until).toBe(2_100_000_000);
    expect(recordFromHello(share, { kind: 'bogus' })).toBeNull();
  });

  test('connect refreshes and persists the record from hello', async () => {
    const until = 1_800_000_000 + 3600; // FakeClock starts at 1.8e12 ms
    const { mgr, h, state } = harness({ ...record, kind: 'share', until: until - 100 });
    state.hello = { kind: 'share', expires_at: until, limit: { workspace: 'w1' } };
    await mgr.start();
    await flush(20);
    expect(h().getSnapshot().status).toBe('online');
    expect(h().getSnapshot().record).toMatchObject({ kind: 'share', until, limit: { workspace: 'w1' } });
    expect(state.puts.at(-1)).toMatchObject({ host_id: 'h1', until });
    mgr.stop();
  });

  test('a hello saying the device already expired stops the connection', async () => {
    const { mgr, h, state, methods } = harness();
    state.hello = { kind: 'share', expires_at: 1, limit: { pane: 'p1' } };
    await mgr.start();
    await flush(20);
    expect(h().getSnapshot().status).toBe('expired');
    expect(methods()).not.toContain('dashboard.get');
    mgr.stop();
  });
});
