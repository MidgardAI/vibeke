// Terminal mirror: ANSI or styled rows → spans (never HTML), find highlighting, wrap and text size.

import { memo, useEffect, useMemo, useRef, type CSSProperties, type ReactNode } from 'react';
import { parseAnsi, type Segment, type Style } from '../lib/ansi';
import { cx } from './ui';

function styleOf(s: Style): CSSProperties | undefined {
  if (!s.fg && !s.bg && !s.bold && !s.dim && !s.italic && !s.underline && !s.inverse && !s.strike) return undefined;
  const css: CSSProperties = {};
  let fg = s.fg;
  let bg = s.bg;
  if (s.inverse) {
    [fg, bg] = [bg ?? 'var(--term-bg)', fg ?? 'var(--term-fg)'];
  }
  if (fg) css.color = fg;
  if (bg) css.backgroundColor = bg;
  if (s.bold) css.fontWeight = 600;
  if (s.dim) css.opacity = 0.6;
  if (s.italic) css.fontStyle = 'italic';
  const deco = [s.underline && 'underline', s.strike && 'line-through'].filter(Boolean).join(' ');
  if (deco) css.textDecoration = deco;
  return css;
}

export interface FindState {
  query: string;
  /** Index of the current match across the screen. */
  current: number;
}

/** Count case-insensitive matches in plain text. */
export function countMatches(text: string, q: string): number {
  if (!q) return 0;
  const hay = text.toLowerCase();
  const needle = q.toLowerCase();
  let n = 0;
  for (let i = hay.indexOf(needle); i >= 0; i = hay.indexOf(needle, i + needle.length)) n++;
  return n;
}

function renderLine(segs: Segment[], q: string, counter: { n: number }, current: number): ReactNode[] {
  if (!q) return segs.map((s, i) => <span key={i} style={styleOf(s.style)}>{s.text}</span>);
  // Find across segment boundaries: work on the joined line, then split marks back into segments.
  const text = segs.map((s) => s.text).join('');
  const lower = text.toLowerCase();
  const needle = q.toLowerCase();
  const hits: [number, number][] = [];
  for (let i = lower.indexOf(needle); i >= 0; i = lower.indexOf(needle, i + needle.length)) hits.push([i, i + needle.length]);
  if (!hits.length) return segs.map((s, i) => <span key={i} style={styleOf(s.style)}>{s.text}</span>);
  const out: ReactNode[] = [];
  let pos = 0;
  let k = 0;
  const hitIds = hits.map(() => counter.n++);
  for (const s of segs) {
    const start = pos;
    const end = pos + s.text.length;
    let cur = start;
    const parts: ReactNode[] = [];
    for (let h = 0; h < hits.length; h++) {
      const [hs, he] = hits[h]!;
      if (he <= cur || hs >= end) continue;
      const a = Math.max(hs, cur);
      const b = Math.min(he, end);
      if (a > cur) parts.push(text.slice(cur, a));
      parts.push(
        <mark key={`m${h}-${a}`} className={cx('find-hit', hitIds[h] === current && 'current')} data-hit={hitIds[h]}>
          {text.slice(a, b)}
        </mark>,
      );
      cur = b;
    }
    if (cur < end) parts.push(text.slice(cur, end));
    out.push(
      <span key={k++} style={styleOf(s.style)}>
        {parts}
      </span>,
    );
    pos = end;
  }
  return out;
}

export const TerminalMirror = memo(function TerminalMirror({
  text,
  lines: styledLines,
  wrap,
  fontSize,
  find,
  className,
  stickToBottom = true,
}: {
  text: string;
  /** Pre-built styled lines (colour mirror); when set, `text` is not parsed. */
  lines?: Segment[][] | null;
  wrap: boolean;
  fontSize: number;
  find?: FindState;
  className?: string;
  stickToBottom?: boolean;
}) {
  const lines = useMemo(() => styledLines ?? parseAnsi(text), [styledLines, text]);
  const ref = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const stickRef = useRef(stickToBottom);
  stickRef.current = stickToBottom && !find?.query;

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const size = { w: el.clientWidth, h: el.clientHeight };
    const onScroll = () => {
      // A scroll caused by a resize (the browser clamping scrollTop while the text rewraps) is not
      // the user scrolling away from the bottom: the resize observer below handles it.
      if (el.clientWidth !== size.w || el.clientHeight !== size.h) return;
      atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    };
    el.addEventListener('scroll', onScroll, { passive: true });
    // A resize (window, belt or dock opening, rewrapping) keeps a bottom-pinned screen pinned.
    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => {
      size.w = el.clientWidth;
      size.h = el.clientHeight;
      if (stickRef.current && atBottom.current) el.scrollTop = el.scrollHeight;
    });
    ro?.observe(el);
    return () => {
      el.removeEventListener('scroll', onScroll);
      ro?.disconnect();
    };
  }, []);

  useEffect(() => {
    const el = ref.current;
    if (el && stickToBottom && atBottom.current && !find?.query) el.scrollTop = el.scrollHeight;
  }, [lines, stickToBottom, find?.query]);

  useEffect(() => {
    if (!find?.query) return;
    const hit = ref.current?.querySelector(`mark[data-hit="${find.current}"]`);
    hit?.scrollIntoView({ block: 'center' });
  }, [find?.query, find?.current, lines]);

  const counter = { n: 0 };
  return (
    <div
      ref={ref}
      className={cx('term overflow-auto px-2 py-1.5', className)}
      style={{ fontSize }}
      aria-label="terminal"
      role="log"
    >
      <pre className={cx('m-0 font-[inherit]', wrap ? 'whitespace-pre-wrap break-all' : 'whitespace-pre')}>
        {lines.map((segs, i) => (
          <div key={i} className="min-h-[1.35em]">
            {renderLine(segs, find?.query ?? '', counter, find?.current ?? -1)}
          </div>
        ))}
      </pre>
    </div>
  );
});
