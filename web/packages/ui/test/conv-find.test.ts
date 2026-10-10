import { describe, expect, test } from 'bun:test';
import type { TranscriptTurn } from '@vibeke/core';
import { countInTurns, findMatches, locateRange, stepHit, turnSearchText, userJumpTarget } from '../src/lib/conv-find';

const turn = (n: number, items: TranscriptTurn['items']): TranscriptTurn => ({ n, items });

describe('findMatches', () => {
  test('case-insensitive, non-overlapping', () => {
    expect(findMatches('Foo foo FOO', 'foo')).toEqual([
      [0, 3],
      [4, 7],
      [8, 11],
    ]);
    expect(findMatches('aaaa', 'aa')).toEqual([
      [0, 2],
      [2, 4],
    ]);
  });
  test('regex characters are literal; empty query matches nothing', () => {
    expect(findMatches('a.c abc a.c', 'a.c')).toEqual([
      [0, 3],
      [8, 11],
    ]);
    expect(findMatches('(x)', '(x')).toEqual([[0, 2]]);
    expect(findMatches('abc', '')).toEqual([]);
  });
});

describe('turn text', () => {
  const t = turn(1, [
    { kind: 'text', role: 'user', text: '<system-reminder>hidden</system-reminder>fix the build' },
    { kind: 'thinking', text: 'secret thoughts about build' },
    { kind: 'tool_call', tool: 'Bash', summary: '{"command":"build"}' },
    { kind: 'text', role: 'assistant', text: 'The build works now.' },
  ]);
  test('only prompts and replies', () => {
    expect(turnSearchText(t)).toBe('fix the build\nThe build works now.');
    expect(countInTurns([t, t], 'build')).toBe(4);
    expect(countInTurns([t], '')).toBe(0);
  });
});

describe('stepHit', () => {
  test('wraps both ways', () => {
    expect(stepHit(2, 3, 1)).toBe(0);
    expect(stepHit(0, 3, -1)).toBe(2);
    expect(stepHit(0, 0, 1)).toBe(-1);
  });
});

describe('locateRange', () => {
  test('inside one node and across nodes', () => {
    expect(locateRange([5, 5], 1, 3)).toEqual({ startNode: 0, startOffset: 1, endNode: 0, endOffset: 3 });
    expect(locateRange([5, 5], 3, 7)).toEqual({ startNode: 0, startOffset: 3, endNode: 1, endOffset: 2 });
  });
  test('a range ending at a node end stays in it; a range starting there moves on', () => {
    expect(locateRange([5, 5], 2, 5)).toEqual({ startNode: 0, startOffset: 2, endNode: 0, endOffset: 5 });
    expect(locateRange([5, 5], 5, 8)).toEqual({ startNode: 1, startOffset: 0, endNode: 1, endOffset: 3 });
  });
  test('outside the text', () => {
    expect(locateRange([3], 4, 6)).toBeNull();
    expect(locateRange([3], 2, 2)).toBeNull();
  });
});

describe('userJumpTarget', () => {
  const tops = [-300, -100, 50, 400];
  test('previous: the last message above the view', () => {
    expect(userJumpTarget(tops, 0, -1)).toBe(1);
    expect(userJumpTarget([10, 20], 0, -1)).toBeNull();
  });
  test('next: the first message below the view', () => {
    expect(userJumpTarget(tops, 0, 1)).toBe(2);
    expect(userJumpTarget(tops, 500, 1)).toBeNull();
  });
  test('a message at the top edge counts as the current one', () => {
    expect(userJumpTarget([-100, 2], 0, -1)).toBe(0);
    expect(userJumpTarget([-100, 2, 300], 0, 1)).toBe(2);
  });
});
