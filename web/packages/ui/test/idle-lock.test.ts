// Idle lock: no timers while hidden; the lock timer resumes on show; locks after the idle period
// only while visible.

import { describe, expect, test } from 'bun:test';
import type { Clock } from '@vibeke/core';
import { IdleLock } from '../src/lib/idle-lock';
import { ValueStore } from '../src/lib/store';

function fakeClock() {
  let t = 0;
  const timers = new Map<number, { at: number; fn: () => void }>();
  let next = 1;
  const clock: Clock = {
    now: () => t,
    setTimeout: (fn, ms) => (timers.set(next, { at: t + ms, fn }), next++),
    clearTimeout: (h) => void timers.delete(h as number),
  };
  const advance = (ms: number) => {
    const target = t + ms;
    for (;;) {
      const due = [...timers].filter(([, e]) => e.at <= target).sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      timers.delete(due[0]);
      t = due[1].at;
      due[1].fn();
    }
    t = target;
  };
  return { clock, timers, advance };
}

const IDLE = 30 * 60_000;

function setup(visible = true) {
  const c = fakeClock();
  const vis = new ValueStore(visible);
  const locked = new ValueStore(false);
  const lock = new IdleLock({ clock: c.clock, visible: vis, locked, idleMs: IDLE }).start();
  return { ...c, vis, locked, lock };
}

describe('idle lock', () => {
  test('locks after the idle period while visible; activity postpones it', () => {
    const { advance, locked, lock, timers } = setup();
    expect(timers.size).toBe(1);
    advance(IDLE - 1000);
    lock.touch();
    advance(1000);
    expect(locked.get()).toBe(false);
    expect(timers.size).toBe(1); // re-armed for the rest, a single timer
    advance(IDLE);
    expect(locked.get()).toBe(true);
    expect(timers.size).toBe(0); // nothing runs while locked
  });

  test('no timers while the window is hidden; the timer resumes on show', () => {
    const { advance, vis, locked, timers } = setup();
    vis.set(false);
    expect(timers.size).toBe(0);
    advance(3 * IDLE); // hidden for hours: never woken, never locked
    expect(locked.get()).toBe(false);
    vis.set(true);
    expect(timers.size).toBe(1);
    advance(IDLE - 1);
    expect(locked.get()).toBe(false); // showing counts as activity
    advance(1);
    expect(locked.get()).toBe(true);
  });

  test('starting hidden arms nothing; unlocking re-arms; stop clears', () => {
    const { vis, locked, lock, timers, advance } = setup(false);
    expect(timers.size).toBe(0);
    vis.set(true);
    advance(IDLE);
    expect(locked.get()).toBe(true);
    locked.set(false);
    expect(timers.size).toBe(1);
    lock.stop();
    expect(timers.size).toBe(0);
    vis.set(false);
    vis.set(true);
    expect(timers.size).toBe(0);
  });
});
