// The agent conversation (spec 16 §9.1, workspace centre): the structured transcript
// (`agent.transcript`) as a reading column — the user's prompts as right-aligned pills, the
// agent's words as Markdown, tool calls as one-line rows (long runs fold into "N steps"), and a
// footer per turn (copy, time worked, counts; the latest turn adds the working tree's +/−).
// Live: when the run's state moves (dashboard refreshes on agent.* events) the newest two turns
// are fetched again (debounced) and merged by `n`; while the agent works they are polled.
// Older turns load when scrolling to the top (`next_before`).

import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  ArrowDown,
  Bot,
  Brain,
  ChevronRight,
  Copy,
  FilePen,
  FilePlus2,
  FileText,
  Globe,
  ListChecks,
  ListTree,
  Loader2,
  Map as MapIcon,
  Search,
  SquareTerminal,
  Wrench,
  X,
} from 'lucide-react';
import { RpcError, applyLatest, applyOlder, emptyTranscript, type AgentRun, type TranscriptItem, type TranscriptState, type TranscriptTurn } from '@vibeke/core';
import { useApp, useVisible } from '../../app/hooks';
import { Markdown } from '../../components/markdown';
import { Button, Chip, DiffCount, Empty, IconButton, Spinner, cx } from '../../components/ui';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { turnBlocks, turnHasWork, turnStats, turnText, workedFor, type ConvBlock, type Step, type ToolStep } from '../../lib/conversation';
import { toolSummary, type ToolKind } from '../../lib/tool-summary';
import { useGitStatus } from '../../lib/use-git-status';

const FIRST_PAGE = 20;
const LIVE_PAGE = 2;
const OLDER_PAGE = 20;
const DEBOUNCE_MS = 300;
const WORKING_POLL_MS = 4000;
const LONG_USER = 600;

/** What changes on the run when a turn starts / ends or its state moves (drives a refetch). */
export const runRevision = (run: AgentRun): string =>
  [run.id, run.execution.value, run.execution.since_ms, run.turns_completed, run.done_rev, run.last_tool ?? '', run.last_message ?? ''].join('|');

const isUnsupported = (e: unknown): boolean =>
  e instanceof RpcError && (e.kind === 'unsupported' || e.kind === 'not_found' || /no transcript/i.test(e.message));

export function Conversation({
  hostId,
  pane,
  run,
  cwd,
  refreshKey,
  onOpenTerminal,
  tail,
}: {
  hostId: string;
  pane: string;
  run: AgentRun;
  cwd: string | null;
  /** Bumped by the parent after sending a prompt: fetch the newest turns soon. */
  refreshKey: number;
  onOpenTerminal(): void;
  /** Rendered after the last turn (open approvals). */
  tail?: ReactNode;
}) {
  const app = useApp();
  const visible = useVisible();
  const [tr, setTr] = useState<TranscriptState>(emptyTranscript);
  const [phase, setPhase] = useState<'loading' | 'ready' | 'none' | 'error'>('loading');
  const [error, setError] = useState<string | null>(null);
  const [olderBusy, setOlderBusy] = useState(false);
  const [atBottom, setAtBottom] = useState(true);
  const listRef = useRef<HTMLDivElement>(null);
  const trRef = useRef(tr);
  trRef.current = tr;
  const pendingScroll = useRef<{ k: 'bottom' } | { k: 'keep'; height: number; top: number } | null>(null);
  const inflight = useRef(false);
  const again = useRef(false);
  const working = run.execution.value === 'working' || run.execution.value === 'starting';

  const nearBottom = () => {
    const el = listRef.current;
    return !el || el.scrollHeight - el.scrollTop - el.clientHeight < 64;
  };

  const loadLatest = useCallback(async () => {
    const conn = app.conn(hostId);
    if (!conn) return;
    if (inflight.current) {
      again.current = true;
      return;
    }
    inflight.current = true;
    try {
      const have = trRef.current.turns.length > 0 && trRef.current.run === run.id;
      const r = await conn.request('agent.transcript', { target: run.id, limit: have ? LIVE_PAGE : FIRST_PAGE });
      if (nearBottom() || !have) pendingScroll.current = { k: 'bottom' };
      setTr((s) => applyLatest(s.run !== null && s.run !== run.id ? emptyTranscript() : s, r));
      setPhase('ready');
      setError(null);
    } catch (e) {
      if (isUnsupported(e)) setPhase('none');
      else if (!trRef.current.turns.length) {
        setPhase('error');
        setError(errorMessage(e));
      }
    } finally {
      inflight.current = false;
      if (again.current) {
        again.current = false;
        void loadLatest();
      }
    }
  }, [app, hostId, run.id]);

  const loadOlder = useCallback(async () => {
    const conn = app.conn(hostId);
    const cur = trRef.current;
    if (!conn || cur.nextBefore === null || olderBusy) return;
    setOlderBusy(true);
    try {
      const r = await conn.request('agent.transcript', { target: run.id, limit: OLDER_PAGE, before: cur.nextBefore });
      const el = listRef.current;
      if (el) pendingScroll.current = { k: 'keep', height: el.scrollHeight, top: el.scrollTop };
      setTr((s) => applyOlder(s, r));
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    } finally {
      setOlderBusy(false);
    }
  }, [app, hostId, run.id, olderBusy]);

  // A different run: start over.
  useEffect(() => {
    setTr(emptyTranscript());
    setPhase('loading');
    void loadLatest();
  }, [run.id]);

  // Turn started / completed / state changed (and sends): refetch the newest turns, debounced.
  const rev = runRevision(run);
  const first = useRef(true);
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    const id = setTimeout(() => void loadLatest(), DEBOUNCE_MS);
    return () => clearTimeout(id);
  }, [rev, refreshKey]);

  // Tool calls do not always reach the dashboard: poll while the agent works and we are seen.
  useEffect(() => {
    if (!working || !visible || phase === 'none') return;
    const id = setInterval(() => void loadLatest(), WORKING_POLL_MS);
    return () => clearInterval(id);
  }, [working, visible, phase, loadLatest]);

  useLayoutEffect(() => {
    const p = pendingScroll.current;
    const el = listRef.current;
    if (!p || !el) return;
    pendingScroll.current = null;
    if (p.k === 'bottom') el.scrollTop = el.scrollHeight;
    else el.scrollTop = p.top + (el.scrollHeight - p.height);
  }, [tr]);

  // New approvals at the tail: keep them in view when following the stream.
  const tailRef = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (atBottom && listRef.current) listRef.current.scrollTop = listRef.current.scrollHeight;
  }, [tail]);

  const onScroll = () => {
    const el = listRef.current;
    if (!el) return;
    setAtBottom(nearBottom());
    if (el.scrollTop < 160 && trRef.current.nextBefore !== null && !olderBusy && phase === 'ready') void loadOlder();
  };

  // +/− of the working tree, shown on the latest turn (shared with the panel's polling).
  const git = useGitStatus(hostId, pane, { poll: false });
  const diff = useMemo(() => {
    let adds = 0;
    let dels = 0;
    for (const f of git.status?.files ?? []) {
      adds += f.adds ?? 0;
      dels += f.dels ?? 0;
    }
    return { adds, dels };
  }, [git.status]);

  const turns = tr.turns;
  const lastN = turns.length ? turns[turns.length - 1]!.n : -1;

  if (phase === 'none')
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        <Empty
          icon={<SquareTerminal />}
          title={t.conv.noTranscript}
          hint={t.conv.noTranscriptHint}
          action={
            <Button icon={<SquareTerminal />} onClick={onOpenTerminal}>
              {t.conv.openTerminal}
            </Button>
          }
        />
        <div className="mx-auto w-full max-w-[780px] px-4 sm:px-6">{tail}</div>
      </div>
    );

  return (
    <div className="relative flex min-h-0 flex-1 flex-col">
      <div ref={listRef} onScroll={onScroll} className="vk-scroll min-h-0 flex-1 overflow-y-auto" role="log" aria-label={t.conv.label} aria-busy={phase === 'loading'}>
        <div className="mx-auto w-full max-w-[780px] px-4 pb-6 pt-3 sm:px-6">
          <div className="flex h-8 items-center justify-center text-xs text-faint">
            {olderBusy ? (
              <span className="inline-flex items-center gap-1.5">
                <Loader2 className="size-3.5 animate-spin" /> {t.conv.loadingOlder}
              </span>
            ) : tr.nextBefore === null && turns.length > 0 ? (
              t.conv.start
            ) : null}
          </div>
          {phase === 'loading' && (
            <div className="flex justify-center py-12">
              <Spinner />
            </div>
          )}
          {phase === 'error' && <div className="py-8 text-center text-sm text-del">{error}</div>}
          {phase === 'ready' && turns.length === 0 && <Empty title={t.conv.empty} hint={t.conv.emptyHint} />}
          {turns.map((turn) => (
            <TurnView
              key={turn.n}
              turn={turn}
              latest={turn.n === lastN}
              working={working && turn.n === lastN}
              cwd={cwd}
              adds={turn.n === lastN ? diff.adds : 0}
              dels={turn.n === lastN ? diff.dels : 0}
            />
          ))}
          {working && phase === 'ready' && (
            <div className="flex h-7 items-center gap-2 text-[13px] text-muted" aria-live="polite">
              <Loader2 className="size-3.5 animate-spin" />
              {t.conv.working}
            </div>
          )}
          {tail && (
            <div ref={tailRef} className="mt-3 space-y-2">
              {tail}
            </div>
          )}
        </div>
      </div>
      {!atBottom && turns.length > 0 && (
        <button
          type="button"
          aria-label={t.conv.jumpLatest}
          title={t.conv.jumpLatest}
          onClick={() => listRef.current?.scrollTo({ top: listRef.current.scrollHeight, behavior: 'smooth' })}
          className="vk-focus absolute bottom-3 left-1/2 inline-flex size-8 -translate-x-1/2 items-center justify-center rounded-full border border-border bg-surface-2 text-muted shadow-[var(--shadow)] hover:text-fg"
        >
          <ArrowDown className="size-4" />
        </button>
      )}
    </div>
  );
}

// ---- one turn --------------------------------------------------------------------------------

const TurnView = memo(
  function TurnView({ turn, latest, working, cwd, adds, dels }: { turn: TranscriptTurn; latest: boolean; working: boolean; cwd: string | null; adds: number; dels: number }) {
    const blocks = useMemo(() => turnBlocks(turn), [turn]);
    const groups = useMemo(() => groupSteps(blocks), [blocks]);
    const lastStepKey = useMemo(() => {
      for (let i = blocks.length - 1; i >= 0; i--) if (blocks[i]!.k === 'tool') return blocks[i]!.key;
      return null;
    }, [blocks]);
    return (
      <section
        id={`turn-${turn.n}`}
        data-turn={turn.n}
        className={cx('space-y-3 py-2', !latest && '[contain-intrinsic-size:auto_320px] [content-visibility:auto]')}
      >
        {groups.map((g) =>
          Array.isArray(g) ? (
            <div key={g[0]!.key} className="-my-0.5">
              {g.map((b) => (
                <StepView key={b.key} block={b} cwd={cwd} pending={working && b.key === lastStepKey} />
              ))}
            </div>
          ) : (
            <BlockView key={g.key} block={g} />
          ),
        )}
        {turnHasWork(turn) && !working && <TurnFooter turn={turn} latest={latest} adds={adds} dels={dels} />}
      </section>
    );
  },
  (a, b) =>
    a.turn.n === b.turn.n &&
    a.turn.items.length === b.turn.items.length &&
    a.turn.duration_ms === b.turn.duration_ms &&
    a.latest === b.latest &&
    a.working === b.working &&
    a.cwd === b.cwd &&
    a.adds === b.adds &&
    a.dels === b.dels &&
    // Results arrive for existing calls without adding items when the server merges; compare the tail item.
    a.turn.items[a.turn.items.length - 1] === b.turn.items[b.turn.items.length - 1],
);

type StepBlock = Step | Extract<ConvBlock, { k: 'steps' }>;
const isStepBlock = (b: ConvBlock): b is StepBlock => b.k === 'tool' || b.k === 'thinking' || b.k === 'steps';

/** Adjacent steps render as one tight group. */
function groupSteps(blocks: ConvBlock[]): (ConvBlock | StepBlock[])[] {
  const out: (ConvBlock | StepBlock[])[] = [];
  for (const b of blocks) {
    const last = out[out.length - 1];
    if (isStepBlock(b)) {
      if (Array.isArray(last)) last.push(b);
      else out.push([b]);
    } else out.push(b);
  }
  return out;
}

function BlockView({ block }: { block: ConvBlock }) {
  if (block.k === 'user') return <UserMessage text={block.text} />;
  if (block.k === 'text') return <Markdown text={block.text} className="text-[14px] leading-[1.65] text-fg" />;
  return null;
}

function UserMessage({ text }: { text: string }) {
  const long = text.length > LONG_USER || text.split('\n').length > 10;
  const [open, setOpen] = useState(!long);
  return (
    <div className="flex justify-end pt-1" data-role="user">
      <div className="max-w-[85%] rounded-2xl bg-surface-2 px-3.5 py-2 text-[14px] leading-relaxed text-fg">
        <div className={cx('whitespace-pre-wrap break-words', !open && 'line-clamp-6')}>{text}</div>
        {long && (
          <button type="button" className="mt-1 text-xs text-muted hover:text-fg" onClick={() => setOpen(!open)}>
            {open ? t.conv.showLess : t.conv.showMore}
          </button>
        )}
      </div>
    </div>
  );
}

// ---- steps -------------------------------------------------------------------------------------

const ICONS: Record<ToolKind, typeof Wrench> = {
  shell: SquareTerminal,
  read: FileText,
  edit: FilePen,
  write: FilePlus2,
  search: Search,
  web: Globe,
  task: Bot,
  todo: ListChecks,
  plan: MapIcon,
  other: Wrench,
};

const rowCls =
  'vk-focus group -mx-1.5 flex h-7 w-[calc(100%+12px)] min-w-0 items-center gap-2 rounded-md px-1.5 text-left text-[13px] hover:bg-hover pointer-coarse:h-9';

function StepView({ block, cwd, pending }: { block: StepBlock; cwd: string | null; pending: boolean }) {
  if (block.k === 'steps') return <FoldedSteps steps={block.steps} cwd={cwd} />;
  if (block.k === 'thinking') return <ThinkingRow text={block.text} />;
  return <ToolRow step={block} cwd={cwd} pending={pending} />;
}

function FoldedSteps({ steps, cwd }: { steps: Step[]; cwd: string | null }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button type="button" className={rowCls} aria-expanded={open} onClick={() => setOpen(!open)}>
        <ListTree className="size-3.5 shrink-0 text-faint" />
        <span className="shrink-0 text-muted">{t.conv.steps(steps.length)}</span>
        <ChevronRight className={cx('size-3.5 shrink-0 text-faint transition-transform', open && 'rotate-90')} />
      </button>
      {open && (
        <div className="ml-[7px] border-l border-border pl-3">
          {steps.map((s) => (s.k === 'thinking' ? <ThinkingRow key={s.key} text={s.text} /> : <ToolRow key={s.key} step={s} cwd={cwd} pending={false} />))}
        </div>
      )}
    </div>
  );
}

function ThinkingRow({ text }: { text: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button type="button" className={rowCls} aria-expanded={open} onClick={() => setOpen(!open)}>
        <Brain className="size-3.5 shrink-0 text-faint" />
        <span className="shrink-0 text-muted">{t.conv.thinking}</span>
        {!open && <span className="min-w-0 flex-1 truncate text-xs italic text-faint">{text.replace(/\s+/g, ' ')}</span>}
      </button>
      {open && <div className="mb-1.5 ml-5.5 whitespace-pre-wrap break-words text-[13px] italic leading-relaxed text-muted">{text}</div>}
    </div>
  );
}

export function ToolRow({ step, cwd, pending }: { step: ToolStep; cwd: string | null; pending: boolean }) {
  const [open, setOpen] = useState(false);
  const call: TranscriptItem | null = step.call;
  const res: TranscriptItem | null = step.result;
  const s = call ? toolSummary(call.tool, call.summary, cwd) : { kind: 'other' as ToolKind, label: t.conv.result, detail: (res?.summary ?? '').replace(/\s+/g, ' ') };
  const Icon = ICONS[s.kind];
  const failed = !!res?.error;
  const input = call?.summary && call.summary !== 'null' ? call.summary : null;
  const output = res?.summary || res?.text || null;
  const can = !!(input || output);
  return (
    <div data-tool={call?.tool ?? 'result'}>
      <button type="button" className={rowCls} aria-expanded={can ? open : undefined} disabled={!can} onClick={() => setOpen(!open)}>
        <Icon className={cx('size-3.5 shrink-0', failed ? 'text-del' : 'text-faint')} />
        <span className={cx('shrink-0', failed ? 'text-del' : 'text-muted')}>{s.label}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-xs text-faint group-hover:text-muted">{s.detail}</span>
        {pending && !res && <Loader2 className="size-3.5 shrink-0 animate-spin text-faint" aria-label={t.conv.working} />}
        {failed && <X className="size-3.5 shrink-0 text-del" aria-label={t.conv.error} />}
      </button>
      {open && can && (
        <div className="mb-1.5 ml-5.5 space-y-1.5 rounded-md border border-border bg-surface px-2.5 py-2">
          {input && <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all font-mono text-xs text-muted">{s.detail && s.kind === 'shell' ? s.detail : input}</pre>}
          {output && (
            <pre className={cx('max-h-64 overflow-auto whitespace-pre-wrap break-all border-t border-border pt-1.5 font-mono text-xs', failed ? 'text-del' : 'text-fg/80', !input && 'border-t-0 pt-0')}>
              {output}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}

// ---- footer --------------------------------------------------------------------------------------

function TurnFooter({ turn, latest, adds, dels }: { turn: TranscriptTurn; latest: boolean; adds: number; dels: number }) {
  const app = useApp();
  const st = turnStats(turn);
  const text = turnText(turn);
  const meta = [st.durationMs !== null ? t.conv.workedFor(workedFor(st.durationMs)) : null, !latest && st.tools > 0 ? t.conv.tools(st.tools) : null].filter(Boolean).join(' · ');
  const chips = latest && (st.tools > 0 || st.subagents > 0 || adds > 0 || dels > 0);
  return (
    <div className="space-y-2" data-turn-footer>
      <div className="-ml-1.5 flex items-center gap-1 text-xs text-faint">
        {text && (
          <IconButton label={t.conv.copy} onClick={() => void app.platform.clipboard.writeText(text).then(() => app.toast(t.copied, 'ok'))}>
            <Copy />
          </IconButton>
        )}
        {meta && <span className={cx('tabular-nums', !text && 'pl-1.5')}>{meta}</span>}
      </div>
      {chips && (
        <div className="flex flex-wrap gap-1.5">
          {st.tools > 0 && <Chip className="h-7 px-2.5">{t.conv.tools(st.tools)}</Chip>}
          {st.subagents > 0 && (
            <Chip className="h-7 px-2.5" icon={<Bot />}>
              {t.conv.subagents(st.subagents)}
            </Chip>
          )}
          {(adds > 0 || dels > 0) && (
            <Chip className="h-7 px-2.5">
              <DiffCount adds={adds} dels={dels} />
            </Chip>
          )}
        </div>
      )}
    </div>
  );
}
