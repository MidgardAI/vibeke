import { describe, expect, test } from 'bun:test';
import type { AppEvent } from '@vibeke/core';
import { EVENT_DEBOUNCE_MS, LatestFeed, isRunEvent, watchRunEvents, type FeedClock } from '../src/lib/live-transcript';

/** Manual clock: timers fire on `advance`. */
function fakeClock() {
  let now = 0;
  let next = 1;
  const timers = new Map<number, { at: number; fn: () => void }>();
  const clock: FeedClock = {
    setTimeout: (fn, ms) => {
      const id = next++;
      timers.set(id, { at: now + ms, fn });
      return id;
    },
    clearTimeout: (h) => void timers.delete(h as number),
  };
  const advance = async (ms: number) => {
    now += ms;
    for (const [id, t] of [...timers].sort((a, b) => a[1].at - b[1].at)) {
      if (t.at <= now && timers.has(id)) {
        timers.delete(id);
        t.fn();
      }
    }
    await flush();
  };
  return { clock, advance, pending: () => timers.size };
}

const flush = async () => {
  for (let i = 0; i < 10; i++) await Promise.resolve();
};

/** A fetch whose answers are released by hand, per call. */
function controlledFetch() {
  const calls: { run: string; resolve: (v: string) => void; reject: (e: unknown) => void }[] = [];
  const fetch = (run: string) =>
    new Promise<string>((resolve, reject) => {
      calls.push({ run, resolve, reject });
    });
  return { fetch, calls };
}

const ev = (type: string, subject: Record<string, string>, seq = 1): AppEvent => ({ seq, ts: 0, type, subject, data: {} });

describe('run events', () => {
  test('the run (or its pane) and the types that change its newest turns', () => {
    for (const type of ['agent.turn_started', 'agent.turn_completed', 'agent.state_changed', 'agent.usage', 'agent.file_changed', 'interaction.opened', 'interaction.resolved'])
      expect(isRunEvent(ev(type, { run: 'r1' }), 'r1', 'p1')).toBe(true);
    expect(isRunEvent(ev('interaction.opened', { pane: 'p1' }), 'r1', 'p1')).toBe(true);
    expect(isRunEvent(ev('agent.turn_started', { run: 'r2', pane: 'p2' }), 'r1', 'p1')).toBe(false);
    expect(isRunEvent(ev('pane.output', { pane: 'p1' }), 'r1', 'p1')).toBe(false);
    expect(isRunEvent(ev('agent.named', { run: 'r1' }), 'r1', 'p1')).toBe(false);
    expect(isRunEvent(ev('interaction.opened', { pane: 'p1' }), 'r1', null)).toBe(false);
  });

  test('watchRunEvents filters a host event source; null without one', () => {
    const subs = new Map<string, Set<(e: AppEvent) => void>>();
    const source = {
      subscribeEvents(host: string, cb: (e: AppEvent) => void) {
        if (!subs.has(host)) subs.set(host, new Set());
        subs.get(host)!.add(cb);
        return () => void subs.get(host)!.delete(cb);
      },
    };
    const emit = (host: string, e: AppEvent) => subs.get(host)?.forEach((cb) => cb(e));
    const hits: string[] = [];
    const off = watchRunEvents(source, 'h1', 'r1', 'p1', (e) => hits.push(e.type))!;
    emit('h1', ev('agent.turn_started', { run: 'r1' }));
    emit('h1', ev('agent.turn_started', { run: 'r9' }));
    emit('h2', ev('agent.turn_started', { run: 'r1' }));
    emit('h1', ev('tab.created', { pane: 'p1' }));
    expect(hits).toEqual(['agent.turn_started']);
    off();
    emit('h1', ev('agent.turn_completed', { run: 'r1' }));
    expect(hits.length).toBe(1);
    expect(watchRunEvents(null, 'h1', 'r1', 'p1', () => {})).toBeNull();
    expect(watchRunEvents({} as never, 'h1', 'r1', 'p1', () => {})).toBeNull();
  });
});

describe('LatestFeed', () => {
  function setup() {
    const c = fakeClock();
    const f = controlledFetch();
    const applied: string[] = [];
    const failed: string[] = [];
    const feed = new LatestFeed<string>({
      fetch: f.fetch,
      apply: (run, r) => applied.push(`${run}=${r}`),
      fail: (run, e) => failed.push(`${run}:${String(e)}`),
      clock: c.clock,
    });
    return { ...c, ...f, applied, failed, feed };
  }

  test('a burst of events costs one debounced request', async () => {
    const s = setup();
    s.feed.setRun('A');
    s.calls[0]!.resolve('a0');
    await flush();
    for (let i = 0; i < 5; i++) s.feed.schedule();
    await s.advance(EVENT_DEBOUNCE_MS - 1);
    expect(s.calls.length).toBe(1);
    await s.advance(1);
    expect(s.calls.length).toBe(2);
    s.calls[1]!.resolve('a1');
    await flush();
    expect(s.applied).toEqual(['A=a0', 'A=a1']);
  });

  test('triggers while a request is in flight queue exactly one follow-up', async () => {
    const s = setup();
    s.feed.setRun('A');
    void s.feed.load();
    void s.feed.load();
    expect(s.calls.length).toBe(1);
    s.calls[0]!.resolve('a0');
    await flush();
    expect(s.calls.length).toBe(2);
    s.calls[1]!.resolve('a1');
    await flush();
    expect(s.calls.length).toBe(2);
    expect(s.applied).toEqual(['A=a0', 'A=a1']);
  });

  test("a delayed answer for run A never lands on run B, and B still loads", async () => {
    const s = setup();
    s.feed.setRun('A');
    void s.feed.load(); // queued follow-up for A
    s.feed.schedule(); // and a pending debounce for A
    s.feed.setRun('B'); // B must fetch at once, despite A's request in flight
    expect(s.calls.map((c) => c.run)).toEqual(['A', 'B']);
    s.calls[0]!.resolve('late-a');
    await flush();
    await s.advance(1000);
    expect(s.applied).toEqual([]);
    expect(s.calls.length).toBe(2); // A's queued follow-up and debounce were dropped
    s.calls[1]!.resolve('b0');
    await flush();
    expect(s.applied).toEqual(['B=b0']);
    // A failure for A after the switch is dropped too.
    s.feed.setRun('A');
    s.feed.setRun('B');
    s.calls[2]!.reject('boom');
    s.calls[3]!.resolve('b1');
    await flush();
    expect(s.failed).toEqual([]);
    expect(s.applied).toEqual(['B=b0', 'B=b1']);
  });

  test('dispose drops the answer in flight and pending timers', async () => {
    const s = setup();
    s.feed.setRun('A');
    s.feed.schedule();
    s.feed.dispose();
    s.calls[0]!.resolve('a0');
    await s.advance(1000);
    expect(s.applied).toEqual([]);
    expect(s.calls.length).toBe(1);
    expect(s.pending()).toBe(0);
  });

  test('errors reach `fail` for the current run', async () => {
    const s = setup();
    s.feed.setRun('A');
    s.calls[0]!.reject('nope');
    await flush();
    expect(s.failed).toEqual(['A:nope']);
    // The loop is free again afterwards.
    void s.feed.load();
    expect(s.calls.length).toBe(2);
  });
});
