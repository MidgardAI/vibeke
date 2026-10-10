// Terminal mirror: ANSI or styled rows → spans (never HTML), find highlighting, wrap and text size.

import { Fragment, memo, useEffect, useMemo, useRef, type CSSProperties, type ReactNode } from 'react';
import { parseAnsi, type Segment, type Style } from '../lib/ansi';
import { findLinks } from '../lib/linkify';
import { FileLink, UrlLink, type LinkOps } from './link-context';
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

interface LineLink {
  start: number;
  end: number;
  url?: string;
  path?: string;
  line?: number;
}

/** URLs, and file paths that exist in the workspace, found in one line of output. */
function lineLinks(text: string, ops: LinkOps): LineLink[] {
  if (text.length < 3 || text.length > 4000) return [];
  const out: LineLink[] = [];
  for (const s of findLinks(text)) {
    if (s.kind === 'url') out.push({ start: s.start, end: s.end, url: s.url });
    else {
      const path = ops.fileFor(s.raw);
      if (path) out.push({ start: s.start, end: s.end, path, line: s.line });
    }
  }
  return out;
}

function renderLine(segs: Segment[], q: string, counter: { n: number }, current: number, links?: LinkOps | null): ReactNode[] {
  const plain = () => segs.map((s, i) => <span key={i} style={styleOf(s.style)}>{s.text}</span>);
  if (!q && !links) return plain();
  // Find and links work across segment boundaries: on the joined line, then cut back into segments.
  const text = segs.map((s) => s.text).join('');
  const hits: [number, number][] = [];
  if (q) {
    const lower = text.toLowerCase();
    const needle = q.toLowerCase();
    for (let i = lower.indexOf(needle); i >= 0; i = lower.indexOf(needle, i + needle.length)) hits.push([i, i + needle.length]);
  }
  const found = links ? lineLinks(text, links) : [];
  if (!hits.length && !found.length) return plain();
  const hitIds = hits.map(() => counter.n++);
  const out: ReactNode[] = [];
  let pos = 0;
  segs.forEach((s, si) => {
    const start = pos;
    const end = pos + s.text.length;
    pos = end;
    const cuts = new Set<number>([start, end]);
    for (const [a, b] of hits) for (const x of [a, b]) if (x > start && x < end) cuts.add(x);
    for (const l of found) for (const x of [l.start, l.end]) if (x > start && x < end) cuts.add(x);
    const points = [...cuts].sort((x, y) => x - y);
    const parts: ReactNode[] = [];
    for (let i = 0; i + 1 < points.length; i++) {
      const a = points[i]!;
      const b = points[i + 1]!;
      let node: ReactNode = text.slice(a, b);
      const h = hits.findIndex(([hs, he]) => hs <= a && b <= he);
      if (h >= 0)
        node = (
          <mark className={cx('find-hit', hitIds[h] === current && 'current')} data-hit={hitIds[h]}>
            {node}
          </mark>
        );
      const l = found.find((x) => x.start <= a && b <= x.end);
      if (l && links)
        node = l.url ? (
          <UrlLink url={l.url} ops={links}>
            {node}
          </UrlLink>
        ) : (
          <FileLink path={l.path!} line={l.line} ops={links}>
            {node}
          </FileLink>
        );
      parts.push(<Fragment key={`${a}`}>{node}</Fragment>);
    }
    out.push(
      <span key={si} style={styleOf(s.style)}>
        {parts}
      </span>,
    );
  });
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
  links,
  onAtBottom,
  jumpRef,
}: {
  text: string;
  /** Pre-built styled lines (colour mirror); when set, `text` is not parsed. */
  lines?: Segment[][] | null;
  wrap: boolean;
  fontSize: number;
  find?: FindState;
  className?: string;
  stickToBottom?: boolean;
  /** URLs and existing workspace files in the output become links. */
  links?: LinkOps | null;
  /** Called when the view moves away from or back to the newest line. */
  onAtBottom?(atBottom: boolean): void;
  /** Filled with a function that scrolls to the newest line. */
  jumpRef?: { current: (() => void) | null };
}) {
  const lines = useMemo(() => styledLines ?? parseAnsi(text), [styledLines, text]);
  const ref = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const stickRef = useRef(stickToBottom);
  stickRef.current = stickToBottom && !find?.query;
  const reportRef = useRef(onAtBottom);
  reportRef.current = onAtBottom;
  const reported = useRef(true);
  const report = () => {
    if (reported.current === atBottom.current) return;
    reported.current = atBottom.current;
    reportRef.current?.(atBottom.current);
  };

  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    const size = { w: el.clientWidth, h: el.clientHeight };
    const onScroll = () => {
      // A scroll caused by a resize (the browser clamping scrollTop while the text rewraps) is not
      // the user scrolling away from the bottom: the resize observer below handles it.
      if (el.clientWidth !== size.w || el.clientHeight !== size.h) return;
      atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
      report();
    };
    el.addEventListener('scroll', onScroll, { passive: true });
    // A resize (window, belt or dock opening, rewrapping) keeps a bottom-pinned screen pinned.
    const ro = typeof ResizeObserver === 'undefined' ? null : new ResizeObserver(() => {
      size.w = el.clientWidth;
      size.h = el.clientHeight;
      if (stickRef.current && atBottom.current) el.scrollTop = el.scrollHeight;
      else atBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
      report();
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
    if (!jumpRef) return;
    jumpRef.current = () => {
      const el = ref.current;
      if (!el) return;
      atBottom.current = true;
      report();
      const calm = typeof window !== 'undefined' && window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
      el.scrollTo({ top: el.scrollHeight, behavior: calm ? 'auto' : 'smooth' });
    };
    return () => {
      jumpRef.current = null;
    };
  }, [jumpRef]);

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
            {renderLine(segs, find?.query ?? '', counter, find?.current ?? -1, links)}
          </div>
        ))}
      </pre>
    </div>
  );
});
