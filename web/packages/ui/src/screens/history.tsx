// History (spec 16 §9.1): the structured transcript (`agent.transcript`), paged backwards with
// `before` and merged by the turn's stable `n`. Items render per kind: messages as bubbles,
// thinking collapsed, tool calls/results as compact expandable rows. Find, and jump between own
// messages.

import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ArrowDownToLine, ArrowUpToLine, Brain, ChevronDown, ChevronRight, ChevronUp, CornerDownRight, Search, Wrench, X } from 'lucide-react';
import {
  applyLatest,
  applyOlder,
  emptyTranscript,
  ownMessages,
  turnMatches,
  type AgentRun,
  type TranscriptItem,
  type TranscriptState,
  type TranscriptTurn,
} from '@vibeke/core';
import { useApp } from '../app/hooks';
import { Markdown } from '../components/markdown';
import { Button, Empty, IconButton, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';

const PAGE = 30;
const COLLAPSE_LINES = 14;

export function HistoryView({ hostId, run }: { hostId: string; run: AgentRun | null }) {
  const app = useApp();
  const [tr, setTr] = useState<TranscriptState>(emptyTranscript);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [query, setQuery] = useState('');
  const [findOpen, setFindOpen] = useState(false);
  const [mine, setMine] = useState(-1);
  const listRef = useRef<HTMLDivElement>(null);
  const trRef = useRef(tr);
  trRef.current = tr;
  /** Scroll to apply after the next render: `bottom`, or keep position after prepending. */
  const pendingScroll = useRef<{ k: 'bottom' } | { k: 'keep'; height: number; top: number } | null>(null);

  const load = async (older: boolean) => {
    const conn = app.conn(hostId);
    if (!conn || !run) return;
    const cur = trRef.current;
    if (older && cur.nextBefore === null) return;
    setBusy(true);
    setError(null);
    try {
      if (older) {
        const r = await conn.request('agent.transcript', { target: run.id, limit: PAGE, before: cur.nextBefore! });
        const el = listRef.current;
        if (el) pendingScroll.current = { k: 'keep', height: el.scrollHeight, top: el.scrollTop };
        setTr((s) => applyOlder(s, r));
      } else {
        const r = await conn.request('agent.transcript', { target: run.id, limit: PAGE });
        const el = listRef.current;
        const atBottom = !el || el.scrollHeight - el.scrollTop - el.clientHeight < 48;
        if (atBottom || !trRef.current.turns.length) pendingScroll.current = { k: 'bottom' };
        setTr((s) => applyLatest(s, r));
      }
    } catch (e) {
      setError(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  useLayoutEffect(() => {
    const p = pendingScroll.current;
    const el = listRef.current;
    if (!p || !el) return;
    pendingScroll.current = null;
    if (p.k === 'bottom') el.scrollTop = el.scrollHeight;
    else el.scrollTop = p.top + (el.scrollHeight - p.height);
  }, [tr]);

  useEffect(() => {
    if (tr.run !== null && tr.run !== run?.id) setTr(emptyTranscript());
    setMine(-1);
  }, [run?.id]);

  useEffect(() => {
    void load(false);
  }, [run?.id, run?.turns_completed]);

  const turns = tr.turns;
  const own = useMemo(() => ownMessages(turns), [turns]);
  const q = query.trim().toLowerCase();
  const shown = useMemo(() => (q ? turns.filter((x) => turnMatches(x, q)) : turns), [turns, q]);

  const jump = (key: string) => document.getElementById(`item-${key}`)?.scrollIntoView({ block: 'start', behavior: 'smooth' });
  const jumpMine = (dir: 1 | -1) => {
    if (!own.length) return;
    const next = mine < 0 || mine >= own.length ? (dir < 0 ? own.length - 1 : 0) : Math.max(0, Math.min(own.length - 1, mine + dir));
    setMine(next);
    jump(own[next]!);
  };

  if (!run) return <Empty title={t.history.empty} />;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-1 border-b border-border px-2 py-1">
        {findOpen ? (
          <>
            <Search className="size-4 text-muted" />
            <input autoFocus value={query} onChange={(e) => setQuery(e.target.value)} placeholder={t.pane.find} className="h-9 min-w-0 flex-1 bg-transparent outline-none" />
            <span className="text-xs text-muted">{q ? t.history.matches(shown.length) : ''}</span>
            <IconButton
              label={t.close}
              onClick={() => {
                setFindOpen(false);
                setQuery('');
              }}
            >
              <X className="size-4" />
            </IconButton>
          </>
        ) : (
          <>
            <IconButton label={t.pane.find} onClick={() => setFindOpen(true)}>
              <Search className="size-4.5" />
            </IconButton>
            <span className="flex-1" />
            <IconButton label={t.history.prevMine} onClick={() => jumpMine(-1)} disabled={!own.length}>
              <ArrowUpToLine className="size-4.5" />
            </IconButton>
            <IconButton label={t.history.nextMine} onClick={() => jumpMine(1)} disabled={!own.length}>
              <ArrowDownToLine className="size-4.5" />
            </IconButton>
          </>
        )}
      </div>
      <div ref={listRef} className="min-h-0 flex-1 overflow-y-auto px-3 py-3">
        <div className="mb-3 flex justify-center">
          {tr.nextBefore !== null ? (
            <Button size="sm" variant="outline" busy={busy} onClick={() => void load(true)}>
              {t.history.loadOlder}
            </Button>
          ) : (
            turns.length > 0 && <span className="text-xs text-faint">{t.history.noOlder}</span>
          )}
        </div>
        {error && <div className="mb-2 text-center text-sm text-danger">{error}</div>}
        {busy && turns.length === 0 && (
          <div className="flex justify-center py-8">
            <Spinner />
          </div>
        )}
        {!busy && !error && turns.length === 0 && <Empty title={t.history.empty} />}
        <div className="space-y-2.5">
          {shown.map((turn) => (
            <Turn key={turn.n} turn={turn} highlight={q} />
          ))}
        </div>
      </div>
    </div>
  );
}

function Turn({ turn, highlight }: { turn: TranscriptTurn; highlight: string }) {
  return (
    <div id={`turn-${turn.n}`} className="space-y-1.5">
      {turn.items.map((it, i) => (
        <Item key={i} id={`item-${turn.n}:${i}`} it={it} highlight={highlight} />
      ))}
    </div>
  );
}

function Item({ it, id, highlight }: { it: TranscriptItem; id: string; highlight: string }) {
  switch (it.kind) {
    case 'text':
      return <Message id={id} it={it} highlight={highlight} />;
    case 'thinking':
      return (
        <Row id={id} icon={<Brain className="size-3.5" />} title={t.history.thinking} open={!!highlight} detail={it.text ?? it.summary ?? null}>
          {null}
        </Row>
      );
    case 'tool_call':
      return (
        <Row id={id} icon={<Wrench className="size-3.5" />} title={it.tool || t.history.tool} open={!!highlight} detail={[it.summary, it.text].filter(Boolean).join('\n\n') || null}>
          {it.summary && <span className="truncate font-mono text-xs text-muted">{it.summary}</span>}
        </Row>
      );
    case 'tool_result':
      return (
        <Row
          id={id}
          icon={<CornerDownRight className="size-3.5" />}
          title={it.error ? t.history.toolError : t.history.toolResult}
          tone={it.error ? 'danger' : undefined}
          open={!!highlight}
          detail={[it.summary, it.text].filter(Boolean).join('\n\n') || null}
        >
          {it.summary && <span className={cx('truncate font-mono text-xs', it.error ? 'text-danger' : 'text-muted')}>{it.summary}</span>}
        </Row>
      );
    default:
      return it.text ? <Row id={id} icon={null} title={String(it.kind)} open={false} detail={it.text}>{null}</Row> : null;
  }
}

function Message({ it, id, highlight }: { it: TranscriptItem; id: string; highlight: string }) {
  const text = it.text ?? '';
  const long = text.split('\n').length > COLLAPSE_LINES || text.length > 1500;
  const [open, setOpen] = useState(!long || !!highlight);
  const user = it.role === 'user';
  if (!text.trim()) return null;
  return (
    <div id={id} className={cx('flex scroll-mt-2', user ? 'justify-end' : 'justify-start')}>
      <div className={cx('max-w-[92%] rounded-2xl px-3 py-2 text-sm', user ? 'bg-accent/15' : 'border border-border bg-surface')}>
        <div className="mb-0.5 text-2xs font-semibold uppercase tracking-wide text-faint">{user ? t.history.you : t.history.agent}</div>
        <div className={cx(!open && 'max-h-40 overflow-hidden')}>
          {user ? <div className="whitespace-pre-wrap break-words">{text}</div> : <Markdown text={text} />}
        </div>
        {long && (
          <button type="button" className="mt-1 flex items-center gap-1 text-xs text-accent" onClick={() => setOpen(!open)}>
            {open ? <ChevronUp className="size-3.5" /> : <ChevronDown className="size-3.5" />}
            {open ? t.history.collapse : t.history.expand}
          </button>
        )}
      </div>
    </div>
  );
}

/** A compact, expandable line for thinking and tool activity. */
function Row({
  id,
  icon,
  title,
  tone,
  open: initial,
  detail,
  children,
}: {
  id: string;
  icon: ReactNode;
  title: string;
  tone?: 'danger';
  open: boolean;
  detail: string | null;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(initial);
  useEffect(() => {
    if (initial) setOpen(true);
  }, [initial]);
  const can = !!detail;
  return (
    <div id={id} className={cx('rounded-lg border px-2 py-1 text-sm', tone === 'danger' ? 'border-danger/40 bg-danger/5' : 'border-border/60')}>
      <button
        type="button"
        disabled={!can}
        aria-expanded={can ? open : undefined}
        onClick={() => setOpen(!open)}
        className={cx('flex w-full min-w-0 items-center gap-1.5 text-left', tone === 'danger' ? 'text-danger' : 'text-muted')}
      >
        {can ? open ? <ChevronDown className="size-3.5 shrink-0" /> : <ChevronRight className="size-3.5 shrink-0" /> : <span className="size-3.5 shrink-0" />}
        <span className="shrink-0">{icon}</span>
        <span className="shrink-0 font-medium">{title}</span>
        {!open && children}
      </button>
      {open && detail && <pre className="mt-1 max-h-80 overflow-auto whitespace-pre-wrap break-words font-mono text-xs text-fg/80">{detail}</pre>}
    </div>
  );
}
