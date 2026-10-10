// Find in the conversation: matching, hit stepping and jumps between the user's own messages.
// The view marks hits in the rendered text (components/…/conversation.tsx); this file holds the
// parts that do not need the DOM.

import type { TranscriptTurn } from '@vibeke/core';
import { cleanUserText } from './conversation';

/** Most hits marked at once. */
export const MAX_HITS = 2000;

const escapeRe = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');

/** Case-insensitive, non-overlapping matches of `q` in `text` as [start, end) offsets. */
export function findMatches(text: string, q: string): [number, number][] {
  if (!q || !text) return [];
  const out: [number, number][] = [];
  for (const m of text.matchAll(new RegExp(escapeRe(q), 'gi'))) {
    out.push([m.index!, m.index! + m[0].length]);
    if (out.length >= MAX_HITS) break;
  }
  return out;
}

/** The text of a turn that the conversation shows without opening anything: prompts and replies. */
export function turnSearchText(turn: TranscriptTurn): string {
  const parts: string[] = [];
  for (const it of turn.items) {
    if (it.kind !== 'text' || !it.text) continue;
    if (it.role === 'user') {
      const t = cleanUserText(it.text);
      if (t) parts.push(t);
    } else if (it.text.trim()) parts.push(it.text);
  }
  return parts.join('\n');
}

/** Hits in the loaded turns (the model's count; the view counts what it rendered). */
export function countInTurns(turns: readonly TranscriptTurn[], q: string): number {
  if (!q) return 0;
  let n = 0;
  for (const t of turns) n += findMatches(turnSearchText(t), q).length;
  return Math.min(n, MAX_HITS);
}

/** Next / previous index in `0..n-1`, wrapping. -1 stays -1 for an empty list. */
export function stepHit(i: number, n: number, dir: 1 | -1): number {
  if (n <= 0) return -1;
  return (((i + dir) % n) + n) % n;
}

/**
 * Where a [start, end) range of concatenated text nodes lands: `lengths` are the node lengths
 * in order. Returns node index and offset for both ends (a range ending exactly at a node's end
 * stays in that node), or null when the range is outside the text.
 */
export function locateRange(lengths: readonly number[], start: number, end: number): { startNode: number; startOffset: number; endNode: number; endOffset: number } | null {
  if (end <= start) return null;
  let pos = 0;
  let sn = -1;
  let so = 0;
  for (let i = 0; i < lengths.length; i++) {
    const len = lengths[i]!;
    if (sn < 0 && start < pos + len) {
      sn = i;
      so = start - pos;
    }
    if (sn >= 0 && end <= pos + len) return { startNode: sn, startOffset: so, endNode: i, endOffset: end - pos };
    pos += len;
  }
  return null;
}

/**
 * Which of the user's messages to jump to. `tops` are the messages' top edges in order
 * (relative to the same origin as `viewTop`, the scroll position). Previous: the last message
 * that starts above the view; next: the first that starts below it. Null when there is none.
 */
export function userJumpTarget(tops: readonly number[], viewTop: number, dir: 1 | -1, slack = 6): number | null {
  if (dir < 0) {
    for (let i = tops.length - 1; i >= 0; i--) if (tops[i]! < viewTop - slack) return i;
    return null;
  }
  for (let i = 0; i < tops.length; i++) if (tops[i]! > viewTop + slack) return i;
  return null;
}
