import { describe, expect, test } from 'bun:test';
import type { Goal } from '@vibeke/core';
import { goalFromStale, goalTone, planSteps, planWaits, progressFraction, sortGoals, stepStatus } from '../src/lib/goals';
import { controllerOf, fitFrame, frameDelayMs, frameSrc, needsReattach, nextSeq, normalizeUrl, sessionsFor, tapToPage } from '../src/lib/screencast';
import { formatRoute, isDeepRoute, parseRoute } from '../src/router';

const goal = (state: string, extra: Partial<Goal> = {}): Goal => ({ id: `g-${state}`, handle: 'g1', title: 'T', text: '', repo: '/r', state: state as Goal['state'], plan: null, plan_rev: 1, approved_rev: null, ...extra });

describe('goal routes', () => {
  test('push link parses and formats', () => {
    expect(parseRoute('#/g/host1/goal 2')).toEqual({ name: 'goal', host: 'host1', goal: 'goal 2' });
    expect(formatRoute({ name: 'goal', host: 'host1', goal: 'goal 2' })).toBe('#/g/host1/goal%202');
    expect(parseRoute('#/goals')).toEqual({ name: 'goals' });
    expect(parseRoute('#/g/host1').name).toBe('not_found');
    expect(isDeepRoute({ name: 'goal', host: 'h', goal: 'g' })).toBe(true);
  });
});

describe('goal helpers', () => {
  test('plan waits only when planned with a plan', () => {
    expect(planWaits(goal('planned', { plan: { steps: [] } }))).toBe(true);
    expect(planWaits(goal('planned'))).toBe(false);
    expect(planWaits(goal('running', { plan: { steps: [] } }))).toBe(false);
  });
  test('steps and statuses', () => {
    const g = goal('running', { plan: { steps: [{ id: 'a', title: 'A', status: 'done' }, { id: 'b', title: 'B' }] } });
    expect(planSteps(g).map((s) => s.id)).toEqual(['a', 'b']);
    expect(stepStatus(planSteps(g)[0]!)).toBe('done');
    expect(stepStatus(planSteps(g)[1]!)).toBeNull();
    expect(planSteps(goal('draft'))).toEqual([]);
  });
  test('progress, tone and order', () => {
    expect(progressFraction({ done: 1, total: 4 })).toBe(0.25);
    expect(progressFraction({ done: 0, total: 0 })).toBeNull();
    expect(goalTone('planned')).toBe('need');
    const sorted = sortGoals([{ goal: goal('done') }, { goal: goal('running') }, { goal: goal('planned') }]);
    expect(sorted.map((v) => v.goal.state)).toEqual(['planned', 'running', 'done']);
  });
  test('stale error data carries the current goal', () => {
    expect(goalFromStale({ goal: goal('planned') })?.state).toBe('planned');
    expect(goalFromStale({})).toBeNull();
    expect(goalFromStale(undefined)).toBeNull();
  });
});

describe('frame math', () => {
  test('fit keeps the aspect ratio inside the box', () => {
    expect(fitFrame({ width: 1280, height: 720 }, { width: 360, height: 600 })).toEqual({ width: 360, height: 202 });
    expect(fitFrame({ width: 400, height: 800 }, { width: 360, height: 300 })).toEqual({ width: 150, height: 300 });
    expect(fitFrame({ width: 0, height: 10 }, { width: 10, height: 10 })).toEqual({ width: 0, height: 0 });
  });
  test('a tap maps to page coordinates', () => {
    const vp = { width: 1280, height: 720 };
    const drawn = { width: 320, height: 180 };
    expect(tapToPage({ x: 160, y: 90 }, drawn, vp)).toEqual({ x: 640, y: 360 });
    expect(tapToPage({ x: 0, y: 0 }, drawn, vp)).toEqual({ x: 0, y: 0 });
    expect(tapToPage({ x: 320, y: 180 }, drawn, vp)).toEqual({ x: 1279, y: 719 });
    expect(tapToPage({ x: 321, y: 10 }, drawn, vp)).toBeNull();
    expect(tapToPage({ x: -1, y: 10 }, drawn, vp)).toBeNull();
    expect(tapToPage({ x: 1, y: 1 }, { width: 0, height: 0 }, vp)).toBeNull();
  });
  test('poll delay stays between 4 and 8 fps', () => {
    expect(frameDelayMs(6)).toBe(167);
    expect(frameDelayMs(30)).toBe(125);
    expect(frameDelayMs(1)).toBe(250);
  });
});

describe('screencast helpers', () => {
  test('frame source and sequence', () => {
    expect(frameSrc({ data_b64: null })).toBeNull();
    expect(frameSrc({ mime: 'image/png', data_b64: 'AAA' })).toBe('data:image/png;base64,AAA');
    expect(frameSrc({ mime: 'text/html', data_b64: 'AAA' })).toBe('data:image/jpeg;base64,AAA');
    expect(nextSeq(3, { seq: 5 })).toBe(5);
    expect(nextSeq(5, { seq: 2 })).toBe(5);
    expect(nextSeq(5, { seq: null })).toBe(5);
  });
  test('reattach only for a dropped attachment', () => {
    expect(needsReattach('conflict')).toBe(true);
    expect(needsReattach('internal')).toBe(false);
  });
  test('who controls', () => {
    expect(controllerOf(true, true)).toBe('you');
    expect(controllerOf(true, false)).toBe('someone');
    expect(controllerOf(false, false)).toBe('agent');
  });
  test('address normalising', () => {
    expect(normalizeUrl('  ')).toBeNull();
    expect(normalizeUrl('example.com/a')).toBe('https://example.com/a');
    expect(normalizeUrl('localhost:3000')).toBe('http://localhost:3000');
    expect(normalizeUrl('http://x.test')).toBe('http://x.test');
  });
  test('sessions on the preview site come first', () => {
    const list = [{ url: 'https://other.test/' }, { url: 'http://localhost:3000/a' }];
    expect(sessionsFor(list, 'http://localhost:3000').map((s) => s.url)).toEqual(['http://localhost:3000/a', 'https://other.test/']);
    expect(sessionsFor(list, 'not a url')).toEqual(list);
  });
});
