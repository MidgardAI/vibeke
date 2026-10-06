import { describe, expect, test } from 'bun:test';
import type { StyledRow, StyledRun } from '@vibeke/core';
import { cellWidth, styledLines, styledRowSegments, termColor } from '../src/lib/styled';

const run = (start: number, len: number, p: Partial<StyledRun> = {}): StyledRun => ({
  start,
  len,
  fg: null,
  bg: null,
  bold: false,
  dim: false,
  italic: false,
  underline: false,
  inverse: false,
  ...p,
});
const row = (text: string, runs: StyledRun[] = []): StyledRow => ({ text, wrapped: false, runs });

describe('styled rows → spans', () => {
  test('palette indices, truecolor and defaults', () => {
    expect(termColor(1)).toBe('var(--ansi-red)');
    expect(termColor(9)).toBe('var(--ansi-bright-red)');
    expect(termColor(16)).toBe('rgb(0,0,0)');
    expect(termColor(196)).toBe('rgb(255,0,0)');
    expect(termColor(232)).toBe('rgb(8,8,8)');
    expect(termColor(255)).toBe('rgb(238,238,238)');
    expect(termColor('#A0b1C2')).toBe('#a0b1c2');
    expect(termColor(null)).toBeUndefined();
    expect(termColor(256)).toBeUndefined();
    expect(termColor('red; background:url(x)')).toBeUndefined();
  });

  test('runs split the row; plain gaps have no style; attributes carried', () => {
    const segs = styledRowSegments(
      row('ab cd ef', [run(0, 2, { fg: 2, bold: true }), run(3, 2, { fg: '#112233', bg: 4, inverse: true, dim: true }), run(6, 2, { italic: true, underline: true })]),
    );
    expect(segs).toEqual([
      { text: 'ab', style: { fg: 'var(--ansi-green)', bold: true } },
      { text: ' ', style: {} },
      { text: 'cd', style: { fg: '#112233', bg: 'var(--ansi-blue)', dim: true, inverse: true } },
      { text: ' ', style: {} },
      { text: 'ef', style: { italic: true, underline: true } },
    ]);
  });

  test('offsets are cells: wide characters take two', () => {
    // "日本" is 2 graphemes / 4 cells; the run at cell 4 styles "x", not "本x".
    expect(styledRowSegments(row('日本xy', [run(4, 1, { fg: 1 })]))).toEqual([
      { text: '日本', style: {} },
      { text: 'x', style: { fg: 'var(--ansi-red)' } },
      { text: 'y', style: {} },
    ]);
    // A run covering a wide char's two cells, then an emoji, then text.
    expect(styledRowSegments(row('a中🙂b', [run(1, 2, { bold: true }), run(5, 1, { fg: 3 })]))).toEqual([
      { text: 'a', style: {} },
      { text: '中', style: { bold: true } },
      { text: '🙂', style: {} },
      { text: 'b', style: { fg: 'var(--ansi-yellow)' } },
    ]);
    expect(cellWidth('a')).toBe(1);
    expect(cellWidth('é')).toBe(1);
    expect(cellWidth('ｱ')).toBe(1); // halfwidth katakana
    expect(cellWidth('Ａ')).toBe(2); // fullwidth
    expect(cellWidth('한')).toBe(2);
    expect(cellWidth('👍🏽')).toBe(2);
    expect(cellWidth('❤️')).toBe(2);
  });

  test('combining marks stay one cell', () => {
    expect(styledRowSegments(row('éx', [run(1, 1, { fg: 1 })]))).toEqual([
      { text: 'é', style: {} },
      { text: 'x', style: { fg: 'var(--ansi-red)' } },
    ]);
  });

  test('trailing blanks trimmed unless they paint; empty trailing rows dropped; controls never pass', () => {
    expect(styledRowSegments(row('ok      '))).toEqual([{ text: 'ok', style: {} }]);
    expect(styledRowSegments(row('ok   ', [run(2, 3, { bg: 1 })]))).toEqual([
      { text: 'ok', style: {} },
      { text: '   ', style: { bg: 'var(--ansi-red)' } },
    ]);
    expect(styledRowSegments(row('a\u001b[31mb'))).toEqual([{ text: 'a[31mb', style: {} }]);
    const { lines, text } = styledLines([row('one', [run(0, 3, { fg: 1 })]), row('  two'), row('   '), row('')]);
    expect(lines.length).toBe(2);
    expect(text).toBe('one\n  two');
  });

  test('malformed rows degrade to plain text', () => {
    expect(styledRowSegments({ text: 'abc', wrapped: false, runs: [run(1, 0, { fg: 1 }), { start: NaN } as StyledRun] })).toEqual([{ text: 'abc', style: {} }]);
    expect(styledLines(null as unknown as StyledRow[])).toEqual({ lines: [], text: '' });
  });
});
