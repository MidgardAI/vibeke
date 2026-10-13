import { describe, expect, test } from 'bun:test';
import type { AnnotationDraft } from '@vibeke/core';
import { formatRoute, parseRoute } from '../src/router';
import { heldDrafts } from '../src/components/held-drafts';
import { incomingId } from '../src/screens/continue';
import { en } from '../src/i18n';

describe('spec 20 routes', () => {
  test('review, continue and attest', () => {
    expect(parseRoute('#/review?d=abc-_')).toEqual({ name: 'review', d: 'abc-_' });
    expect(parseRoute('#/review')).toEqual({ name: 'review', d: null });
    expect(parseRoute('#/continue?d=xyz')).toEqual({ name: 'continue', d: 'xyz' });
    expect(parseRoute('#/attest?t=v1.a.b')).toEqual({ name: 'attest', query: 't=v1.a.b' });
    expect(parseRoute('#/attest?error=denied')).toEqual({ name: 'attest', query: 'error=denied' });
    for (const h of ['#/review?d=abc', '#/continue?d=xyz', '#/attest?t=v1.a.b', '#/attest']) expect(formatRoute(parseRoute(h))).toBe(h);
  });
});

describe('held review notes', () => {
  const d = (id: string, state: AnnotationDraft['state'], at: number): AnnotationDraft => ({ id, target: { preview: 'p' }, url: 'u', notes: [], state, updated_at_ms: at });
  test('only held drafts, newest first', () => {
    expect(heldDrafts([d('a', 'held', 1), d('b', 'open', 2), d('c', 'held', 3), d('e', 'delivered', 4)]).map((x) => x.id)).toEqual(['c', 'a']);
    expect(heldDrafts(undefined)).toEqual([]);
  });
});

describe('continue jobs', () => {
  test('incoming id from the job', () => {
    expect(incomingId({ job: 'j', state: 'done', incoming: 'in1' })).toBe('in1');
    expect(incomingId({ job: 'j', state: 'done', incoming: { id: 'in2' } })).toBe('in2');
    expect(incomingId({ job: 'j', state: 'done' })).toBeNull();
  });
});

describe('who a refused link is for', () => {
  test('people and teams (spec 20 §2.5)', () => {
    expect(en.pair.github.linkFor(['alice', 'bob'])).toBe('This link is for: @alice, @bob');
    expect(en.pair.github.linkFor(['alice'], ['Acme'])).toBe('This link is for: @alice and everyone in Acme');
    expect(en.pair.github.linkFor([], ['Acme', 'Design'])).toBe('This link is for: everyone in Acme, Design');
  });
});
