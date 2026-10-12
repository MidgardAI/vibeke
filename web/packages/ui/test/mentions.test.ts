import { describe, expect, test } from 'bun:test';
import type { MentionEntry } from '@vibeke/core';
import { dashboard, interaction, pane, run } from './fixtures';
import { atQuery, mentionablePanes, filterMentions, insertMention, mentionRows, suggestMention, validMention } from '../src/lib/mentions';

const none = () => false;

const entry = (name: string, pane: string, p: Partial<MentionEntry> = {}): MentionEntry => ({
  name,
  workspace: 'w1',
  pane,
  kind: 'agent',
  delivery: true,
  created_at_ms: 0,
  label: name,
  harness: 'claude',
  state: 'idle',
  ...p,
});

describe('validMention', () => {
  test('accepts the name rule and refuses reserved words', () => {
    expect(validMention('reviewer')).toBe(true);
    expect(validMention('a')).toBe(true);
    expect(validMention('fix-login_2')).toBe(true);
    expect(validMention('a'.repeat(32))).toBe(true);
    expect(validMention('a'.repeat(33))).toBe(false);
    expect(validMention('')).toBe(false);
    expect(validMention('1abc')).toBe(false);
    expect(validMention('Abc')).toBe(false);
    expect(validMention('a b')).toBe(false);
    expect(validMention('current')).toBe(false);
    expect(validMention('focused')).toBe(false);
    expect(validMention('vibeke')).toBe(false);
    expect(validMention('reviewer_')).toBe(false);
    expect(validMention('reviewer-')).toBe(false);
  });
});

describe('suggestMention', () => {
  test('takes the first words of the label, lowercase', () => {
    expect(suggestMention('Fix Login Redirect', 'claude', none)).toBe('fix-login-redirect');
    expect(suggestMention('Fix the login redirect bug now', 'claude', none)).toBe('fix-login-redirect');
  });

  test('drops filler words unless nothing else is left', () => {
    expect(suggestMention('the review of a PR', 'x', none)).toBe('review-pr');
    expect(suggestMention('the', 'claude', none)).toBe('the');
  });

  test('stops adding words past about 20 characters', () => {
    expect(suggestMention('internationalization configuration', 'x', none)).toBe('internationalization');
    expect(suggestMention('alpha beta gamma delta', 'x', none)).toBe('alpha-beta-gamma');
  });

  test('cuts a very long first word to 32 characters', () => {
    const out = suggestMention('x'.repeat(40), 'y', none);
    expect(out).toBe('x'.repeat(32));
    expect(validMention(out)).toBe(true);
  });

  test('falls back to the fallback, then to "agent"', () => {
    expect(suggestMention('', 'codex', none)).toBe('codex');
    expect(suggestMention('???', 'my-project', none)).toBe('my-project');
    expect(suggestMention('', '', none)).toBe('agent');
    expect(suggestMention('日本語', '', none)).toBe('agent');
  });

  test('skips leading digits and refuses reserved results', () => {
    expect(suggestMention('2 fast tests', 'x', none)).toBe('fast-tests');
    expect(suggestMention('current', 'codex', none)).toBe('codex');
  });

  test('adds -2, -3 for taken names', () => {
    const taken = new Set(['reviewer', 'reviewer-2']);
    expect(suggestMention('Reviewer', 'x', (n) => taken.has(n))).toBe('reviewer-3');
  });

  test('keeps a suffixed name within 32 characters', () => {
    const base = 'x'.repeat(32);
    const out = suggestMention(base, 'y', (n) => n === base);
    expect(out).toBe(`${'x'.repeat(30)}-2`);
    expect(out.length).toBe(32);
  });
});

describe('atQuery', () => {
  test('finds the word at the end of the text', () => {
    expect(atQuery('@')).toEqual({ query: '', start: 0 });
    expect(atQuery('@rev')).toEqual({ query: 'rev', start: 0 });
    expect(atQuery('please ask @re')).toEqual({ query: 're', start: 11 });
    expect(atQuery('(@a')).toEqual({ query: 'a', start: 1 });
  });

  test('ignores emails, finished words and plain text', () => {
    expect(atQuery('mail me@example')).toBeNull();
    expect(atQuery('@reviewer ')).toBeNull();
    expect(atQuery('hello')).toBeNull();
    expect(atQuery('@Rev')).toBeNull();
  });
});

describe('mentionRows', () => {
  test('lists named panes, then unnamed ones with proposals', () => {
    const rows = mentionRows(
      [entry('reviewer', 'p2'), entry('me', 'p1')],
      [
        { id: 'p2', label: 'Review', fallback: 'codex', state: 'idle' },
        { id: 'p3', label: 'Reviewer', fallback: 'codex', state: 'busy' },
        { id: 'p4', label: 'Reviewer', fallback: 'codex', state: 'shell' },
      ],
      'p1',
    );
    expect(rows.map((r) => [r.name, r.named, r.pane])).toEqual([
      ['reviewer', true, 'p2'],
      ['reviewer-2', false, 'p3'],
      ['reviewer-3', false, 'p4'],
    ]);
  });

  test('leaves out the pane being typed in', () => {
    expect(mentionRows([], [{ id: 'p1', label: 'x', fallback: 'y', state: 'idle' }], 'p1')).toEqual([]);
  });
});

describe('filterMentions and insertMention', () => {
  const rows = mentionRows(
    [entry('reviewer', 'p2', { label: 'Code review' }), entry('builder', 'p3', { label: 'Build it' })],
    [],
    'p1',
  );

  test('names that start with the query come first', () => {
    expect(filterMentions(rows, 'r').map((r) => r.name)).toEqual(['reviewer', 'builder']);
    expect(filterMentions(rows, '').map((r) => r.name)).toEqual(['reviewer', 'builder']);
    expect(filterMentions(rows, 'zzz')).toEqual([]);
    expect(filterMentions(rows, 'code').map((r) => r.name)).toEqual(['reviewer']);
  });

  test('inserts the name and a trailing space', () => {
    expect(insertMention('ask @re', 4, 'reviewer')).toBe('ask @reviewer ');
    expect(insertMention('@', 0, 'a')).toBe('@a ');
  });
});

describe('mentionablePanes', () => {
  test('labels, fallbacks and states come from the dashboard', () => {
    const d = dashboard({
      panes: [pane({ id: 'p1' }), pane({ id: 'p2', cwd: '/home/dev/api/', auto_title: 'zsh' }), pane({ id: 'p3', workspace: 'w2' }), pane({ id: 'p4', title: 'Docs' })],
      runs: [run({ id: 'r1', pane: 'p1', name: 'coder', harness: 'claude' }), run({ id: 'r4', pane: 'p4', harness: 'codex', execution: { value: 'working', since_ms: 0, source: 'structured', confidence: 1, detail: null } })],
      interactions: [interaction({ pane: 'p1', status: 'open' })],
    });
    expect(mentionablePanes(d, 'w1')).toEqual([
      { id: 'p1', label: 'coder', fallback: 'claude', state: 'needs_input' },
      { id: 'p2', label: 'zsh', fallback: 'api', state: 'shell' },
      { id: 'p4', label: 'Docs', fallback: 'codex', state: 'busy' },
    ]);
  });
});
