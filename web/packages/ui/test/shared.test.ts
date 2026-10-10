import { describe, expect, test } from 'bun:test';
import { appendToDraft, sharedText } from '../src/lib/shared';
import { setNewAgentPrefill, takeNewAgentPrefill } from '../src/lib/new-agent-prefill';
import { formatRoute, parseRoute } from '../src/router';

describe('shared text', () => {
  test('joins title, text and url without repeating', () => {
    expect(sharedText({ title: 'Page', text: 'Read this', url: 'https://example.com/a' })).toBe('Page\nRead this\nhttps://example.com/a');
    expect(sharedText({ title: '', text: 'See https://example.com/a', url: 'https://example.com/a' })).toBe('See https://example.com/a');
    expect(sharedText({ title: 'Same', text: 'Same words', url: '' })).toBe('Same words');
    expect(sharedText({ title: '', text: '', url: '' })).toBe('');
  });
  test('appends to a draft on a new line', () => {
    expect(appendToDraft('', 'x')).toBe('x');
    expect(appendToDraft('hello  \n', 'x')).toBe('hello\nx');
    expect(appendToDraft('hello', '')).toBe('hello');
  });
});

describe('new agent prefill', () => {
  test('is taken once', () => {
    setNewAgentPrefill({ prompt: 'p' });
    expect(takeNewAgentPrefill()).toEqual({ prompt: 'p' });
    expect(takeNewAgentPrefill()).toBeNull();
  });
});

describe('share-in route', () => {
  test('round trips', () => {
    expect(parseRoute('#/share-in/abc')).toEqual({ name: 'share_in', id: 'abc' });
    expect(formatRoute({ name: 'share_in', id: 'a b' })).toBe('#/share-in/a%20b');
    expect(parseRoute('#/share-in').name).toBe('not_found');
  });
});
