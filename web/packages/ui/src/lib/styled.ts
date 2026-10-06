// Styled pane rows (`pane.read` source `styled`, server gateway_api.rs `styled_rows`) → the same
// styled segments the ANSI parser produces, so the mirror renders spans, never HTML.
//
// Run offsets are terminal cells: `start`/`len` count columns, and a row's `text` holds one
// grapheme per cell with the spacer cell of a wide character omitted. So a CJK character or an
// emoji is one grapheme but two cells; offsets are mapped back through the width of each grapheme.

import type { StyledRow, StyledRun } from '@vibeke/core';
import { x256, type Segment, type Style } from './ansi';

/** Palette index (0–15 theme vars, 16–255 xterm cube/greys) or `#rrggbb`; null/invalid = default. */
export function termColor(c: number | string | null | undefined): string | undefined {
  if (typeof c === 'number') return Number.isInteger(c) && c >= 0 && c <= 255 ? x256(c) : undefined;
  if (typeof c === 'string' && /^#[0-9a-fA-F]{6}$/.test(c)) return c.toLowerCase();
  return undefined;
}

const WIDE: [number, number][] = [
  [0x1100, 0x115f],
  [0x231a, 0x231b],
  [0x2329, 0x232a],
  [0x23e9, 0x23ec],
  [0x23f0, 0x23f0],
  [0x23f3, 0x23f3],
  [0x25fd, 0x25fe],
  [0x2614, 0x2615],
  [0x2648, 0x2653],
  [0x267f, 0x267f],
  [0x2693, 0x2693],
  [0x26a1, 0x26a1],
  [0x26aa, 0x26ab],
  [0x26bd, 0x26be],
  [0x26c4, 0x26c5],
  [0x26ce, 0x26ce],
  [0x26d4, 0x26d4],
  [0x26ea, 0x26ea],
  [0x26f2, 0x26f3],
  [0x26f5, 0x26f5],
  [0x26fa, 0x26fa],
  [0x26fd, 0x26fd],
  [0x2705, 0x2705],
  [0x270a, 0x270b],
  [0x2728, 0x2728],
  [0x274c, 0x274c],
  [0x274e, 0x274e],
  [0x2753, 0x2755],
  [0x2757, 0x2757],
  [0x2795, 0x2797],
  [0x27b0, 0x27b0],
  [0x27bf, 0x27bf],
  [0x2b1b, 0x2b1c],
  [0x2b50, 0x2b50],
  [0x2b55, 0x2b55],
  [0x2e80, 0x303e],
  [0x3041, 0x33ff],
  [0x3400, 0x4dbf],
  [0x4e00, 0x9fff],
  [0xa000, 0xa4cf],
  [0xa960, 0xa97f],
  [0xac00, 0xd7a3],
  [0xf900, 0xfaff],
  [0xfe10, 0xfe19],
  [0xfe30, 0xfe6f],
  [0xff00, 0xff60],
  [0xffe0, 0xffe6],
  [0x16fe0, 0x18cff],
  [0x1b000, 0x1b2ff],
  [0x1f004, 0x1f004],
  [0x1f0cf, 0x1f0cf],
  [0x1f18e, 0x1f18e],
  [0x1f191, 0x1f19a],
  [0x1f200, 0x1f251],
  [0x1f300, 0x1f64f],
  [0x1f680, 0x1f6ff],
  [0x1f7e0, 0x1f7eb],
  [0x1f90c, 0x1f9ff],
  [0x1fa70, 0x1faff],
  [0x20000, 0x3fffd],
];

function wideCodePoint(cp: number): boolean {
  let lo = 0;
  let hi = WIDE.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const [a, b] = WIDE[mid]!;
    if (cp < a) hi = mid - 1;
    else if (cp > b) lo = mid + 1;
    else return true;
  }
  return false;
}

/** Terminal cells a grapheme covers: 2 for East Asian wide/fullwidth and emoji presentation. */
export function cellWidth(g: string): 1 | 2 {
  const cp = g.codePointAt(0) ?? 0;
  if (wideCodePoint(cp)) return 2;
  // Text-default pictographs shown as emoji (VS16), and flag pairs.
  if (g.includes('️') && cp >= 0x2000) return 2;
  if (cp >= 0x1f1e6 && cp <= 0x1f1ff && [...g].length > 1) return 2;
  return 1;
}

type Seg = { segment(s: string): Iterable<{ segment: string }> };
let segmenter: Seg | null | undefined;

/** Split into graphemes (Intl.Segmenter when present, else code points). */
export function graphemes(s: string): string[] {
  if (segmenter === undefined) {
    const I = (globalThis as { Intl?: { Segmenter?: new (l?: string, o?: { granularity: string }) => Seg } }).Intl;
    segmenter = I?.Segmenter ? new I.Segmenter(undefined, { granularity: 'grapheme' }) : null;
  }
  if (!segmenter) return Array.from(s);
  return Array.from(segmenter.segment(s), (x) => x.segment);
}

function runStyle(r: StyledRun): Style {
  const st: Style = {};
  const fg = termColor(r.fg);
  const bg = termColor(r.bg);
  if (fg) st.fg = fg;
  if (bg) st.bg = bg;
  if (r.bold) st.bold = true;
  if (r.dim) st.dim = true;
  if (r.italic) st.italic = true;
  if (r.underline) st.underline = true;
  if (r.inverse) st.inverse = true;
  return st;
}

const sameStyle = (a: Style, b: Style) => JSON.stringify(a) === JSON.stringify(b);
const paints = (s: Style) => !!(s.bg || s.inverse || s.underline);

/** One styled row → segments. Trailing blanks without a visible background are trimmed. */
export function styledRowSegments(row: StyledRow): Segment[] {
  const text = typeof row.text === 'string' ? row.text : '';
  const runs = (Array.isArray(row.runs) ? row.runs : [])
    .filter((r) => r && Number.isFinite(r.start) && Number.isFinite(r.len) && r.len > 0)
    .map((r) => ({ start: r.start, end: r.start + r.len, style: runStyle(r) }))
    .sort((a, b) => a.start - b.start);
  const segs: Segment[] = [];
  let col = 0;
  let ri = 0;
  for (const g of graphemes(text)) {
    // Control characters never reach the DOM as-is.
    const shown = g === '\t' ? ' ' : /[\u0000-\u001f\u007f]/.test(g) ? '' : g;
    while (ri < runs.length && runs[ri]!.end <= col) ri++;
    const run = runs[ri];
    const style = run && run.start <= col ? run.style : {};
    col += cellWidth(g);
    if (!shown) continue;
    const last = segs[segs.length - 1];
    if (last && sameStyle(last.style, style)) last.text += shown;
    else segs.push({ text: shown, style });
  }
  // Trim trailing whitespace that paints nothing (keeps wrap mode from spilling blank lines).
  while (segs.length) {
    const last = segs[segs.length - 1]!;
    if (paints(last.style)) break;
    const trimmed = last.text.replace(/\s+$/, '');
    if (trimmed) {
      last.text = trimmed;
      break;
    }
    segs.pop();
  }
  return segs;
}

/** Rows → lines of segments plus plain text (for find/copy); trailing empty rows dropped. */
export function styledLines(rows: readonly StyledRow[]): { lines: Segment[][]; text: string } {
  const lines = (Array.isArray(rows) ? rows : []).map(styledRowSegments);
  while (lines.length && lines[lines.length - 1]!.length === 0) lines.pop();
  const text = lines.map((l) => l.map((s) => s.text).join('')).join('\n');
  return { lines, text };
}
