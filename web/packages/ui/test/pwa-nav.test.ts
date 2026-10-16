import { describe, expect, test } from 'bun:test';
import { FREEZE_QUIET_MS, freezeSections, isFrozen, unfreezeDelay } from '../src/lib/stable-order';
import { compactDashboard, overlayHosts, shouldSave, staleInfo, SAVE_EVERY_MS } from '../src/lib/offline-cache';
import { claimPrefetch } from '../src/lib/prefetch';
import { getScroll, saveScroll } from '../src/lib/scroll-memory';
import { dashboard, host, interaction } from './fixtures';

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
