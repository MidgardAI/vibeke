import { describe, expect, test } from 'bun:test';
import { cacheLabel, cacheStatus, cacheTtlMs, createSharedClock, runCacheStatus, COLD_VISIBLE_MS } from '../src/lib/cache-clock';
import { FREEZE_QUIET_MS, freezeSections, isFrozen, unfreezeDelay } from '../src/lib/stable-order';
import { compactDashboard, overlayHosts, shouldSave, staleInfo, SAVE_EVERY_MS } from '../src/lib/offline-cache';
import { claimPrefetch } from '../src/lib/prefetch';
import { getScroll, saveScroll } from '../src/lib/scroll-memory';
import { ValueStore } from '../src/lib/store';
import { parsePrefs } from '../src/lib/prefs';
import { dashboard, host, interaction, run } from './fixtures';

const MIN = 60_000;
const idle = (since: number, extra = {}) => run({ execution: { value: 'idle', since_ms: since, source: 'structured', confidence: 1, detail: null }, turns_completed: 2, ...extra });

describe('prompt-cache countdown', () => {
  test('default time-to-live per harness, overrides, and unknown harnesses', () => {
    expect(cacheTtlMs('claude')).toBe(5 * MIN);
    expect(cacheTtlMs('codex')).toBe(5 * MIN);
    expect(cacheTtlMs('pi')).toBeNull();
    expect(cacheTtlMs('claude', { claude: 60 })).toBe(60 * MIN);
    expect(cacheTtlMs('claude', { claude: 0 })).toBe(5 * MIN);
    expect(cacheTtlMs('pi', { pi: 10 })).toBe(10 * MIN);
  });

  test('state follows the remaining fraction', () => {
    const ttl = 5 * MIN;
    expect(cacheStatus(0, 0, ttl)).toMatchObject({ state: 'warm', remainingMs: ttl, fraction: 1 });
    expect(cacheStatus(0, 3 * MIN, ttl).state).toBe('warm');
    // Exactly one quarter left is already the warning state.
    expect(cacheStatus(0, 3.75 * MIN, ttl).state).toBe('low');
    expect(cacheStatus(0, ttl - 1, ttl)).toMatchObject({ state: 'low', remainingMs: 1 });
    expect(cacheStatus(0, ttl, ttl)).toMatchObject({ state: 'cold', remainingMs: 0, coldForMs: 0 });
    expect(cacheStatus(0, ttl + 2 * MIN, ttl)).toMatchObject({ state: 'cold', coldForMs: 2 * MIN });
  });

  test('labels round up and say <1m under a minute', () => {
    expect(cacheLabel(5 * MIN)).toBe('5m');
    expect(cacheLabel(4 * MIN + 1)).toBe('5m');
    expect(cacheLabel(MIN)).toBe('1m');
    expect(cacheLabel(MIN - 1)).toBe('<1m');
    expect(cacheLabel(1)).toBe('<1m');
    expect(cacheLabel(60 * MIN)).toBe('1h');
    expect(cacheLabel(65 * MIN)).toBe('1h 5m');
  });

  test('only an idle, live run that finished a turn has a chip', () => {
    const now = 10 * MIN;
    expect(runCacheStatus(idle(9 * MIN), now)).toMatchObject({ state: 'warm', sinceMs: 9 * MIN, ttlMs: 5 * MIN });
    expect(runCacheStatus(idle(9 * MIN, { turns_completed: 0 }), now)).toBeNull();
    expect(runCacheStatus(idle(9 * MIN, { ended_at_ms: 9 * MIN }), now)).toBeNull();
    expect(runCacheStatus(idle(9 * MIN, { harness: 'pi' }), now)).toBeNull();
    expect(runCacheStatus(idle(0), now)).toBeNull();
    const working = run({ execution: { value: 'working', since_ms: 9 * MIN, source: 'structured', confidence: 1, detail: null }, turns_completed: 2 });
    expect(runCacheStatus(working, now)).toBeNull();
    expect(runCacheStatus(null, now)).toBeNull();
  });

  test('a cold chip goes away after an hour', () => {
    const since = 1000;
    expect(runCacheStatus(idle(since), since + 5 * MIN + COLD_VISIBLE_MS)?.state).toBe('cold');
    expect(runCacheStatus(idle(since), since + 5 * MIN + COLD_VISIBLE_MS + 1)).toBeNull();
  });

  test('the shared clock runs only while listened to and visible', () => {
    let t = 0;
    const timers: { f: () => void; live: boolean }[] = [];
    const visible = new ValueStore(true);
    const clock = createSharedClock({
      now: () => t,
      visible,
      setInterval: (f) => {
        const h = { f, live: true };
        timers.push(h);
        return h;
      },
      clearInterval: (h) => void ((h as { live: boolean }).live = false),
    });
    const live = () => timers.filter((x) => x.live).length;
    let calls = 0;
    expect(live()).toBe(0);
    const off1 = clock.subscribe(() => calls++);
    const off2 = clock.subscribe(() => calls++);
    expect(live()).toBe(1);
    t = 5;
    timers[0]!.f();
    expect(clock.getSnapshot()).toBe(5);
    visible.set(false);
    expect(live()).toBe(0);
    t = 99;
    visible.set(true);
    expect(live()).toBe(1);
    expect(clock.getSnapshot()).toBe(99);
    off1();
    expect(live()).toBe(1);
    off2();
    expect(live()).toBe(0);
    expect(calls).toBeGreaterThan(0);
  });

  test('cache TTL overrides are parsed leniently', () => {
    expect(parsePrefs(JSON.stringify({ cacheTtl: { claude: 60, codex: -3, pi: 'x', big: 9999 } })).cacheTtl).toEqual({ claude: 60 });
    expect(parsePrefs(null).cacheTtl).toEqual({});
  });
});

describe('stable list order', () => {
  test('frozen while a pointer is down or briefly after the last touch', () => {
    expect(isFrozen({ pointerDown: true, lastActivityMs: 0, nowMs: 5000 })).toBe(true);
    expect(isFrozen({ pointerDown: false, lastActivityMs: 0, nowMs: 5000 })).toBe(false);
    expect(isFrozen({ pointerDown: false, lastActivityMs: 4000, nowMs: 4000 + FREEZE_QUIET_MS - 1 })).toBe(true);
    expect(isFrozen({ pointerDown: false, lastActivityMs: 4000, nowMs: 4000 + FREEZE_QUIET_MS })).toBe(false);
  });

  test('unfreeze delay is the remaining quiet time', () => {
    expect(unfreezeDelay({ pointerDown: true, lastActivityMs: 1, nowMs: 2 })).toBeNull();
    expect(unfreezeDelay({ pointerDown: false, lastActivityMs: 0, nowMs: 100 })).toBeNull();
    const d = unfreezeDelay({ pointerDown: false, lastActivityMs: 1000, nowMs: 1500 })!;
    expect(d).toBeGreaterThanOrEqual(FREEZE_QUIET_MS - 500);
    expect(d).toBeLessThan(FREEZE_QUIET_MS);
  });

  type R = { key: string; n: number };
  const row = (key: string, n = 0): R => ({ key, n });
  const prev = [
    { id: 'needs', rows: [row('a'), row('b')] },
    { id: 'working', rows: [row('c')] },
  ];

  test('no previous layout: the new one is used', () => {
    const next = [{ id: 'x', rows: [row('z')] }];
    expect(freezeSections(null, next)).toEqual(next);
  });

  test('order stays, fresh data shows in place', () => {
    const next = [
      { id: 'needs', rows: [row('b', 1), row('a', 1)] },
      { id: 'working', rows: [row('c', 1)] },
    ];
    const out = freezeSections(prev, next);
    expect(out.map((s) => s.rows.map((r) => r.key))).toEqual([['a', 'b'], ['c']]);
    expect(out.flatMap((s) => s.rows.map((r) => r.n))).toEqual([1, 1, 1]);
  });

  test('a row that changed section stays put with its fresh data', () => {
    const next = [
      { id: 'needs', rows: [row('c', 2), row('a', 2)] },
      { id: 'working', rows: [row('b', 2)] },
    ];
    const out = freezeSections(prev, next);
    expect(out.map((s) => s.rows.map((r) => r.key))).toEqual([['a', 'b'], ['c']]);
    expect(out[0]!.rows[1]!.n).toBe(2);
  });

  test('removed rows disappear, new rows and sections wait', () => {
    const next = [
      { id: 'needs', rows: [row('new'), row('a')] },
      { id: 'review', rows: [row('fresh')] },
    ];
    const out = freezeSections(prev, next);
    expect(out.map((s) => s.rows.map((r) => r.key))).toEqual([['a']]);
  });
});

describe('offline cold start', () => {
  const d = dashboard({ interactions: [interaction()], previews: [] });

  test('compact drops open interactions and previews', () => {
    const c = compactDashboard(d);
    expect(c.interactions).toEqual([]);
    expect('previews' in c).toBe(false);
    expect(c.workspaces).toEqual(d.workspaces);
  });

  test('overlay fills only hosts without a live dashboard', () => {
    const hosts = [host('h1', null, 'offline'), host('h2', d), host('h3', null)];
    const out = overlayHosts(hosts, { h1: { at: 77, dashboard: compactDashboard(d) }, h2: { at: 5, dashboard: compactDashboard(d) } });
    expect(out[0]!.dashboard).not.toBeNull();
    expect(out[0]!.cachedAt).toBe(77);
    expect(out[1]).toBe(hosts[1]!);
    expect(out[2]!.dashboard).toBeNull();
    expect(overlayHosts(hosts, {})).toBe(hosts);
  });

  test('stale info: dim and "as of" only when offline with data', () => {
    expect(staleInfo(host('h', d))).toEqual({ stale: false, asOf: null });
    expect(staleInfo(host('h', null, 'offline'))).toEqual({ stale: false, asOf: null });
    const off = { ...host('h', d, 'offline'), lastOnlineAt: 40 };
    expect(staleInfo(off)).toEqual({ stale: true, asOf: 40 });
    expect(staleInfo({ ...off, cachedAt: 90 })).toEqual({ stale: true, asOf: 90 });
  });

  test('saves are throttled and only for live data', () => {
    const live = host('h', d);
    expect(shouldSave(live, undefined, 1000)).toBe(true);
    expect(shouldSave(live, 1000, 1000 + SAVE_EVERY_MS - 1)).toBe(false);
    expect(shouldSave(live, 1000, 1000 + SAVE_EVERY_MS)).toBe(true);
    expect(shouldSave(host('h', d, 'offline'), undefined, 1000)).toBe(false);
    expect(shouldSave({ ...live, cachedAt: 5 }, undefined, 1000)).toBe(false);
  });
});

describe('prefetch and scroll memory', () => {
  test('a prefetch key is claimed once per cooldown', () => {
    const m = new Map<string, number>();
    expect(claimPrefetch(m, 'k', 1000)).toBe(true);
    expect(claimPrefetch(m, 'k', 2000)).toBe(false);
    expect(claimPrefetch(m, 'other', 2000)).toBe(true);
    expect(claimPrefetch(m, 'k', 1000 + 8000)).toBe(true);
  });

  test('scroll positions are remembered; zero forgets', () => {
    expect(getScroll('inbox-test')).toBe(0);
    saveScroll('inbox-test', 312.4);
    expect(getScroll('inbox-test')).toBe(312);
    saveScroll('inbox-test', 0);
    expect(getScroll('inbox-test')).toBe(0);
  });
});
