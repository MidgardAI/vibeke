// The agent conversation (spec 16 §9.1, workspace centre): the structured transcript
// (`agent.transcript`) as a reading column — the user's prompts as right-aligned pills, the
// agent's words as Markdown, tool calls as one-line rows (long runs fold into "N steps"), and a
// footer per turn (copy, time worked, counts; the latest turn adds the working tree's +/−).
// Live: the run's host events (turn started / completed, state changes, usage, file edits,
// interactions) refetch the newest two turns (debounced) and merge them by `n`; a change of the
// run's dashboard fields does too (fallback), and a slow safety poll runs while the agent works.
// Older turns load when scrolling to the top (`next_before`).

import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import {
  ArrowDown,
  Bell,
  Bot,
  Brain,
  Check,
  ChevronRight,
  ChevronsDown,
  ChevronsUp,
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
  Square,
  SquareTerminal,
  Wrench,
  X,
} from 'lucide-react';
import { NotConnectedError, RpcError, applyLatest, applyOlder, emptyTranscript, type AgentRun, type BgTask, type AppApi, type TranscriptItem, type TranscriptState, type TranscriptTurn } from '@vibeke/core';
import { useApp, useHost, useNow, useVisible } from '../../app/hooks';
import { FindBar } from '../../components/find-bar';
import { LinkContext, linkifyText, useLinks } from '../../components/link-context';
import { Markdown } from '../../components/markdown';
import { Button, Chip, DiffCount, Empty, IconButton, Spinner, cx } from '../../components/ui';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { ImageSourceContext, TranscriptImage } from '../../components/transcript-image';
import { turnBlocks, turnHasWork, turnStats, turnText, turnToolIds, workedFor, type BgEventStatus, type ConvBlock, type Step, type ToolStep } from '../../lib/conversation';
import { bgTasksOf, bgToolIds } from '../../lib/bg-tasks';
import { shortDuration } from '../../lib/format';
import { toolSummary, type ToolKind } from '../../lib/tool-summary';
import { takePrefetchedTranscript } from '../../lib/prefetch';
import { conversationKey, transcriptMatchesRun, EVENT_DEBOUNCE_MS, LatestFeed, SAFETY_POLL_MS, watchRunEvents } from '../../lib/live-transcript';
import { useGitStatus } from '../../lib/use-git-status';
import { MAX_HITS, findMatches, locateRange, stepHit, turnSearchText, userJumpTarget } from '../../lib/conv-find';
import { usePathLinks } from './use-path-links';

const FIRST_PAGE = 20;
const LIVE_PAGE = 2;
const OLDER_PAGE = 20;
const DEBOUNCE_MS = 250;
const WORKING_POLL_MS = 4000;
const LONG_USER = 600;
/** Older pages loaded on its own while a search finds nothing (20 turns each). */
const MAX_AUTO_PAGES = 15;
/** Text of the newest turns scanned for file paths to link. */
const LINK_SCAN_CHARS = 60_000;

/** The block-level element a text node sits in (a hit never spans two). */
const blockOf = (n: Node): Element | null => n.parentElement?.closest('p,li,pre,h1,h2,h3,h4,blockquote,[data-find-scope]') ?? null;

const highlightsApi = (): { set(k: string, v: unknown): void; delete(k: string): void } | null => {
  const hl = typeof CSS !== 'undefined' ? (CSS as unknown as { highlights?: { set(k: string, v: unknown): void; delete(k: string): void } }).highlights : undefined;
  return hl && typeof (globalThis as { Highlight?: unknown }).Highlight === 'function' ? hl : null;
};

/** What changes on the run when a turn starts / ends or its state moves (drives a refetch). */
export const runRevision = (run: AgentRun): string =>
  [
    run.id,
    run.execution.value,
    run.execution.since_ms,
    run.turns_completed,
    run.done_rev,
    run.last_tool ?? '',
    run.last_message ?? '',
    // The transcript file becomes known (SessionStart) after the run first shows up.
    run.transcript_path ?? '',
    run.harness_session_id ?? '',
  ].join('|');

const isUnsupported = (e: unknown): boolean =>
  e instanceof RpcError && (e.kind === 'unsupported' || e.kind === 'not_found' || /no transcript/i.test(e.message));

export function Conversation(props: Parameters<typeof ConversationSession>[0]) {
  return <ConversationSession key={`${props.hostId}/${conversationKey(props.run)}`} {...props} />;
}

function ConversationSession({
  hostId,
  pane,
  run,
  cwd,
  refreshKey,
  onOpenTerminal,
  tail,
  findOpen = false,
  setFindOpen,
  openFile,
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
  /** The find bar (header search icon, `/`, ⌘F) and its setter. */
  findOpen?: boolean;
  setFindOpen?(v: boolean): void;
  /** Open a workspace file in the viewer; without it paths in the text are not links. */
  openFile?: (path: string, line?: number) => void;
}) {
  const app = useApp();
  const visible = useVisible();
  const online = useHost(hostId)?.status === 'online';
  const [tr, setTr] = useState<TranscriptState>(emptyTranscript);
  const [phase, setPhase] = useState<'loading' | 'ready' | 'none' | 'error'>('loading');
  const [error, setError] = useState<string | null>(null);
  const [olderBusy, setOlderBusy] = useState(false);
  const [atBottom, setAtBottom] = useState(true);
  const listRef = useRef<HTMLDivElement>(null);
  const trRef = useRef(tr);
  trRef.current = tr;
  const pendingScroll = useRef<{ k: 'bottom' } | { k: 'keep'; height: number; top: number } | null>(null);
  const working = run.execution.value === 'working' || run.execution.value === 'starting';
  const runRef = useRef(run.id);
  runRef.current = run.id;
  const currentRun = useRef(run);
  currentRun.current = run;

  const bgKey = bgTasksOf(run).map((b) => `${b.id}:${b.tool_use_id ?? ''}`).join('|');
  // eslint-disable-next-line react-hooks/exhaustive-deps
  const bgIds = useMemo(() => bgToolIds(run), [bgKey]);

  const imageSession = conversationKey(run);
  const imageSource = useMemo(() => ({ host: hostId, target: run.id, session: imageSession }), [hostId, run.id, imageSession]);

  /** Scroll to the tool row a background task started (when it is in the loaded, unfolded turns). */
  const jumpToTool = useCallback((id: string) => {
    const el = listRef.current?.querySelector<HTMLElement>(`[data-tool-id="${CSS.escape(id)}"]`);
    el?.scrollIntoView?.({ block: 'center', behavior: 'smooth' });
  }, []);

  const nearBottom = () => {
    const el = listRef.current;
    return !el || el.scrollHeight - el.scrollTop - el.clientHeight < 64;
  };

  // The newest turns: one request at a time, responses fenced by run (a late answer for the
  // previous run is dropped; see LatestFeed).
  const feed = useMemo(
    () =>
      new LatestFeed<{ r: AppApi['agent.transcript']['result']; have: boolean }>({
        fetch: async (target) => {
          const conn = app.conn(hostId);
          if (!conn) throw new NotConnectedError(hostId);
          const have = trRef.current.turns.length > 0 && trRef.current.run === target;
          // The first page may already be here: a finger on the row started the fetch (lib/prefetch.ts).
          const prefetched = have ? null : takePrefetchedTranscript(hostId, target, app.platform.clock.now());
          // Legacy pages carry no session identity, so they cannot safely warm a new session.
          const warm = prefetched && prefetched.session_id !== undefined && transcriptMatchesRun(prefetched, currentRun.current) ? prefetched : null;
          const r = warm ?? (await conn.request('agent.transcript', { target, limit: have ? LIVE_PAGE : FIRST_PAGE }));
          return { r, have };
        },
        apply: (target, { r, have }) => {
          if (target !== runRef.current || !transcriptMatchesRun(r, currentRun.current)) return;
          if (nearBottom() || !have) pendingScroll.current = { k: 'bottom' };
          setTr((s) => applyLatest(s.run !== null && s.run !== target ? emptyTranscript() : s, r));
          setPhase('ready');
          setError(null);
        },
        fail: (target, e) => {
          if (target !== runRef.current) return;
          if (isUnsupported(e)) setPhase('none');
          // Not connected yet (e.g. the app just resumed): keep loading, the reconnect refetches.
          else if (e instanceof NotConnectedError) {
            if (!trRef.current.turns.length || trRef.current.run !== target) setPhase('loading');
          } else if (!trRef.current.turns.length || trRef.current.run !== target) {
            setPhase('error');
            setError(errorMessage(e));
          }
        },
        debounceMs: EVENT_DEBOUNCE_MS,
      }),
    [app, hostId],
  );
  useEffect(() => () => feed.dispose(), [feed]);

  const loadOlder = useCallback(async () => {
    const conn = app.conn(hostId);
    const cur = trRef.current;
    if (!conn || cur.nextBefore === null || olderBusy) return;
    const target = run.id;
    setOlderBusy(true);
    try {
      const r = await conn.request('agent.transcript', { target, limit: OLDER_PAGE, before: cur.nextBefore });
      if (target !== runRef.current || !transcriptMatchesRun(r, currentRun.current)) return;
      const el = listRef.current;
      if (el) pendingScroll.current = { k: 'keep', height: el.scrollHeight, top: el.scrollTop };
      setTr((s) => (s.run === target ? applyOlder(s, r) : s));
    } catch (e) {
      if (target === runRef.current) app.toast(errorMessage(e), 'error');
    } finally {
      setOlderBusy(false);
    }
  }, [app, hostId, run.id, olderBusy]);

  // A different run: start over.
  useEffect(() => {
    setTr(emptyTranscript());
    setPhase('loading');
    feed.setRun(run.id);
  }, [run.id, feed]);

  // Back online (resumed, network change): fetch the newest turns, a request made while the
  // host was reconnecting failed and turns may have landed meanwhile.
  const wasOnline = useRef(online);
  useEffect(() => {
    if (online && !wasOnline.current) void feed.load();
    wasOnline.current = online;
  }, [online, feed]);

  // Live: the run's events (turn started / completed, state, usage, file edits, approvals).
  const [eventsLive, setEventsLive] = useState(false);
  useEffect(() => {
    const off = watchRunEvents(app.manager, hostId, run.id, pane, () => feed.schedule());
    setEventsLive(!!off);
    return () => off?.();
  }, [app, hostId, run.id, pane, feed]);

  // Fallback: the run's dashboard fields moved (and sends) — refetch the newest turns, debounced.
  const rev = runRevision(run);
  const first = useRef(true);
  useEffect(() => {
    if (first.current) {
      first.current = false;
      return;
    }
    feed.schedule(DEBOUNCE_MS);
  }, [rev, refreshKey]);

  // Safety poll while the agent works and we are seen: slow when events flow, faster without.
  useEffect(() => {
    if (!working || !visible || phase === 'none') return;
    const id = setInterval(() => void feed.load(), eventsLive ? SAFETY_POLL_MS : WORKING_POLL_MS);
    return () => clearInterval(id);
  }, [working, visible, phase, eventsLive, feed]);

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

  // Following the stream: stay at the bottom when the content or the window changes size.
  const atBottomRef = useRef(true);
  const contentRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const el = listRef.current;
    const inner = contentRef.current;
    if (!el || !inner || typeof ResizeObserver === 'undefined') return;
    const ro = new ResizeObserver(() => {
      if (atBottomRef.current) el.scrollTop = el.scrollHeight;
    });
    ro.observe(el);
    ro.observe(inner);
    return () => ro.disconnect();
  }, [phase]);

  const onScroll = () => {
    const el = listRef.current;
    if (!el) return;
    const bottom = nearBottom();
    atBottomRef.current = bottom;
    setAtBottom(bottom);
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

  // ---- links: URLs and workspace files in the text ----
  const linkText = useMemo(() => {
    let out = '';
    for (let i = turns.length - 1; i >= 0 && out.length < LINK_SCAN_CHARS; i--) out += `${turnSearchText(turns[i]!)}\n`;
    return out;
  }, [turns]);
  const links = usePathLinks(hostId, pane, cwd, linkText, openFile);

  // ---- find: hits are ranges over the rendered text, marked with the CSS Highlight API (a
  // class on the hit's element where the browser lacks it). The newest hit comes first; going
  // back past the first one loads older messages, and so does a search that finds nothing.
  const [query, setQuery] = useState('');
  const [hit, setHit] = useState(0);
  const [count, setCount] = useState(0);
  const [scanned, setScanned] = useState<TranscriptState | null>(null);
  const [seek, setSeek] = useState<{ from: number } | null>(null);
  const ranges = useRef<Range[]>([]);
  const fresh = useRef(false);
  const autoPages = useRef(0);
  const marked = useRef<Element | null>(null);
  const scrolledFor = useRef('');

  const clearMarks = useCallback(() => {
    const hl = highlightsApi();
    hl?.delete('vk-find');
    hl?.delete('vk-find-current');
    marked.current?.classList.remove('find-fallback');
    marked.current = null;
  }, []);
  useEffect(() => clearMarks, [clearMarks]);

  const onQuery = (q: string) => {
    setQuery(q);
    setHit(0);
    fresh.current = true;
    autoPages.current = 0;
    scrolledFor.current = '';
    setSeek(null);
  };
  const closeFind = () => {
    setFindOpen?.(false);
    setQuery('');
    setSeek(null);
  };

  useEffect(() => {
    const list = listRef.current;
    if (!findOpen || !query || !list) {
      ranges.current = [];
      clearMarks();
      setCount(0);
      setScanned(tr);
      return;
    }
    const found: Range[] = [];
    for (const scope of list.querySelectorAll('[data-find-scope]')) {
      const nodes: (Text | null)[] = [];
      const lengths: number[] = [];
      let full = '';
      let prev: Element | null | undefined;
      const walker = document.createTreeWalker(scope, NodeFilter.SHOW_TEXT);
      for (let n = walker.nextNode(); n; n = walker.nextNode()) {
        const b = blockOf(n);
        // A new block reads as a line break, so a hit never joins the end of one paragraph to the next.
        if (prev !== undefined && b !== prev) {
          nodes.push(null);
          lengths.push(1);
          full += '\n';
        }
        prev = b;
        const v = (n as Text).data;
        nodes.push(n as Text);
        lengths.push(v.length);
        full += v;
      }
      for (const [a, b] of findMatches(full, query)) {
        const loc = locateRange(lengths, a, b);
        const from = loc ? nodes[loc.startNode] : null;
        const to = loc ? nodes[loc.endNode] : null;
        if (!loc || !from || !to) continue;
        const r = document.createRange();
        r.setStart(from, loc.startOffset);
        r.setEnd(to, loc.endOffset);
        found.push(r);
        if (found.length >= MAX_HITS) break;
      }
      if (found.length >= MAX_HITS) break;
    }
    ranges.current = found;
    const hl = highlightsApi();
    const Hl = (globalThis as unknown as { Highlight?: new (...r: Range[]) => unknown }).Highlight;
    if (hl && Hl) hl.set('vk-find', new Hl(...found));
    setCount(found.length);
    setScanned(tr);
    if (fresh.current) {
      fresh.current = false;
      setHit(Math.max(0, found.length - 1));
    }
  }, [tr, query, findOpen, phase, clearMarks]);

  // The current hit: marked, and scrolled to the middle when the hit (not just the text) changed.
  useEffect(() => {
    const r = ranges.current[hit];
    const hl = highlightsApi();
    const Hl = (globalThis as unknown as { Highlight?: new (...r: Range[]) => unknown }).Highlight;
    marked.current?.classList.remove('find-fallback');
    marked.current = null;
    if (!findOpen || !query || !r) {
      hl?.delete('vk-find-current');
      return;
    }
    if (hl && Hl) hl.set('vk-find-current', new Hl(r));
    else {
      marked.current = r.startContainer.parentElement;
      marked.current?.classList.add('find-fallback');
    }
    const key = `${hit}:${query}`;
    const list = listRef.current;
    if (!list || scrolledFor.current === key) return;
    scrolledFor.current = key;
    const rect = r.getBoundingClientRect();
    const box = list.getBoundingClientRect();
    list.scrollTop += rect.top - box.top - (box.height - rect.height) / 2;
  }, [hit, count, scanned, findOpen, query]);

  // Seeking: look further back for hits until one turns up (or the start is reached).
  useEffect(() => {
    if (!findOpen || !query || olderBusy || scanned !== tr) return;
    if (!seek) {
      if (count === 0 && tr.nextBefore !== null && autoPages.current < MAX_AUTO_PAGES) setSeek({ from: 0 });
      return;
    }
    if (count > seek.from) {
      setHit(count - seek.from - 1);
      setSeek(null);
    } else if (tr.nextBefore !== null && autoPages.current < MAX_AUTO_PAGES) {
      autoPages.current++;
      void loadOlder();
    } else {
      setSeek(null);
      if (count > 0) setHit(count - 1);
    }
  }, [findOpen, query, olderBusy, scanned, tr, seek, count, loadOlder]);

  const stepFind = (dir: 1 | -1) => {
    if (!count) return;
    if (dir < 0 && hit <= 0 && tr.nextBefore !== null) {
      autoPages.current = 0;
      setSeek({ from: count });
      return;
    }
    setHit((h) => stepHit(h, count, dir));
  };
  const findNote = !query ? (tr.nextBefore !== null ? t.conv.findLoaded : null) : seek || olderBusy ? t.conv.findLoading : count === 0 ? t.conv.findNone : tr.nextBefore !== null ? t.conv.findLoaded : null;

  // ---- jumps between the messages the user sent ----
  const jumpAfterLoad = useRef(false);
  const jumpUser = useCallback(
    (dir: 1 | -1, load = true) => {
      const list = listRef.current;
      if (!list) return;
      const nodes = [...list.querySelectorAll<HTMLElement>('[data-role="user"]')];
      const box = list.getBoundingClientRect();
      const tops = nodes.map((n) => n.getBoundingClientRect().top);
      const i = userJumpTarget(tops, box.top, dir);
      if (i === null) {
        if (load && dir < 0 && trRef.current.nextBefore !== null) {
          jumpAfterLoad.current = true;
          void loadOlder();
        }
        return;
      }
      const calm = window.matchMedia?.('(prefers-reduced-motion: reduce)').matches;
      list.scrollTo({ top: list.scrollTop + tops[i]! - box.top - 8, behavior: calm ? 'auto' : 'smooth' });
    },
    [loadOlder],
  );
  useEffect(() => {
    if (!jumpAfterLoad.current || olderBusy) return;
    jumpAfterLoad.current = false;
    jumpUser(-1, false);
  }, [tr, olderBusy, jumpUser]);

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
    <LinkContext.Provider value={links}>
    <ImageSourceContext.Provider value={imageSource}>
    <div className="relative flex min-h-0 flex-1 flex-col">
      {findOpen && (
        <FindBar query={query} onQuery={onQuery} index={hit} count={count} onStep={stepFind} onClose={closeFind} placeholder={t.conv.findPlaceholder} note={findNote} />
      )}
      <div ref={listRef} onScroll={onScroll} className="vk-scroll min-h-0 flex-1 overflow-y-auto" role="log" aria-label={t.conv.label} aria-busy={phase === 'loading'}>
        <div ref={contentRef} className="mx-auto w-full max-w-[780px] px-4 pb-6 pt-3 sm:px-6">
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
              bgIds={bgIds}
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
          {phase === 'ready' && <BgStrip run={run} onJump={jumpToTool} />}
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
      {turns.length > 1 && (
        <div className="absolute bottom-3 right-3 flex flex-col gap-1">
          {([-1, 1] as const).map((dir) => (
            <button
              key={dir}
              type="button"
              aria-label={dir < 0 ? t.conv.prevSent : t.conv.nextSent}
              title={dir < 0 ? t.conv.prevSent : t.conv.nextSent}
              onClick={() => jumpUser(dir)}
              className="vk-focus inline-flex size-8 items-center justify-center rounded-full border border-border bg-surface-2 text-muted opacity-80 shadow-[var(--shadow)] hover:text-fg hover:opacity-100 pointer-coarse:size-10"
            >
              {dir < 0 ? <ChevronsUp className="size-4" /> : <ChevronsDown className="size-4" />}
            </button>
          ))}
        </div>
      )}
    </div>
    </ImageSourceContext.Provider>
    </LinkContext.Provider>
  );
}

// ---- one turn --------------------------------------------------------------------------------

const TurnView = memo(
  function TurnView({ turn, latest, working, cwd, bgIds, adds, dels }: { turn: TranscriptTurn; latest: boolean; working: boolean; cwd: string | null; bgIds: ReadonlySet<string>; adds: number; dels: number }) {
    const blocks = useMemo(() => turnBlocks(turn, !working), [turn, working]);
    const groups = useMemo(() => groupSteps(blocks), [blocks]);
    const lastStepKey = useMemo(() => {
      for (let i = blocks.length - 1; i >= 0; i--) if (blocks[i]!.k === 'tool') return blocks[i]!.key;
      return null;
    }, [blocks]);
    const bgRunning = useMemo(() => {
      if (!bgIds.size) return 0;
      let n = 0;
      for (const id of turnToolIds(turn)) if (bgIds.has(id)) n++;
      return n;
    }, [turn, bgIds]);
    return (
      <section
        id={`turn-${turn.n}`}
        data-turn={turn.n}
        className={cx('flex flex-col gap-3 py-2', !latest && '[contain-intrinsic-size:auto_320px] [content-visibility:auto]')}
      >
        {groups.map((g) =>
          Array.isArray(g) ? (
            <div key={g[0]!.key}>
              {g.map((b) => (
                <StepView key={b.key} block={b} cwd={cwd} bgIds={bgIds} pending={working && b.key === lastStepKey} />
              ))}
            </div>
          ) : (
            <BlockView key={g.key} block={g} />
          ),
        )}
        {(turnHasWork(turn) || bgRunning > 0) && !working && <TurnFooter turn={turn} latest={latest} adds={adds} dels={dels} bgRunning={bgRunning} />}
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
    a.bgIds === b.bgIds &&
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
  if (block.k === 'bg_event') return <BgEventRow status={block.status} summary={block.summary} />;
  if (block.k === 'image') return <TranscriptImage mime={block.mime} imageRef={block.ref} size={block.size} />;
  if (block.k === 'text')
    return (
      <div data-find-scope>
        <Markdown text={block.text} className="text-[14px] leading-[1.65] text-fg" />
      </div>
    );
  return null;
}

function UserMessage({ text }: { text: string }) {
  const ops = useLinks();
  const long = text.length > LONG_USER || text.split('\n').length > 10;
  const [open, setOpen] = useState(!long);
  return (
    <div className="flex justify-end pt-1" data-role="user">
      <div className="max-w-[85%] rounded-2xl bg-surface-2 px-3.5 py-2 text-[14px] leading-relaxed text-fg">
        <div data-find-scope className={cx('whitespace-pre-wrap break-words', !open && 'line-clamp-6')}>
          {ops ? linkifyText(text, ops) : text}
        </div>
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

function StepView({ block, cwd, bgIds, pending }: { block: StepBlock; cwd: string | null; bgIds: ReadonlySet<string>; pending: boolean }) {
  if (block.k === 'steps') return <FoldedSteps steps={block.steps} cwd={cwd} bgIds={bgIds} />;
  if (block.k === 'thinking') return <ThinkingRow text={block.text} />;
  return <ToolRow step={block} cwd={cwd} pending={pending} bgIds={bgIds} />;
}

function FoldedSteps({ steps, cwd, bgIds }: { steps: Step[]; cwd: string | null; bgIds: ReadonlySet<string> }) {
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
          {steps.map((s) => (s.k === 'thinking' ? <ThinkingRow key={s.key} text={s.text} /> : <ToolRow key={s.key} step={s} cwd={cwd} pending={false} bgIds={bgIds} />))}
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

export function ToolRow({ step, cwd, pending, bgIds }: { step: ToolStep; cwd: string | null; pending: boolean; bgIds?: ReadonlySet<string> }) {
  const [open, setOpen] = useState(false);
  const call: TranscriptItem | null = step.call;
  const res: TranscriptItem | null = step.result;
  const s = call ? toolSummary(call.tool, call.text ?? call.summary, cwd) : { kind: 'other' as ToolKind, label: t.conv.result, detail: (res?.summary ?? '').replace(/\s+/g, ' ') };
  const Icon = ICONS[s.kind];
  const failed = !!res?.error;
  const rawInput = call?.text ?? call?.summary;
  const input = rawInput && rawInput !== 'null' ? rawInput : null;
  const output = res?.text || res?.summary || null;
  const can = !!(input || output);
  const inBg = !!(call?.id && bgIds?.has(call.id));
  return (
    <div data-tool={call?.tool ?? 'result'} data-tool-id={call?.id ?? undefined}>
      <button type="button" className={rowCls} aria-expanded={can ? open : undefined} disabled={!can} onClick={() => setOpen(!open)}>
        <Icon className={cx('size-3.5 shrink-0', failed ? 'text-del' : 'text-faint')} />
        <span className={cx('shrink-0', failed ? 'text-del' : 'text-muted')}>{s.label}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-xs text-faint group-hover:text-muted">{s.detail}</span>
        {inBg && (
          <span className="flex shrink-0 items-center gap-1 text-xs text-info">
            <Loader2 className="size-3.5 animate-spin" />
            {t.bg.runningInBackground}
          </span>
        )}
        {!inBg && pending && !res && <Loader2 className="size-3.5 shrink-0 animate-spin text-faint" aria-label={t.conv.working} />}
        {failed && <X className="size-3.5 shrink-0 text-del" aria-label={t.conv.error} />}
      </button>
      {open && can && (
        <div className="mb-1.5 ml-5.5 space-y-1.5 rounded-md border border-border bg-surface px-2.5 py-2">
          {input && <pre className="max-h-48 overflow-auto whitespace-pre-wrap break-all font-mono text-xs text-muted">{input}</pre>}
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

function TurnFooter({ turn, latest, adds, dels, bgRunning }: { turn: TranscriptTurn; latest: boolean; adds: number; dels: number; bgRunning: number }) {
  const app = useApp();
  const st = turnStats(turn);
  const text = turnText(turn);
  const meta = [st.durationMs !== null ? t.conv.workedFor(workedFor(st.durationMs)) : null, !latest && st.tools > 0 ? t.conv.tools(st.tools) : null].filter(Boolean).join(' · ');
  const chips = bgRunning > 0 || (latest && (st.tools > 0 || st.subagents > 0 || adds > 0 || dels > 0));
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
          {bgRunning > 0 && (
            <Chip className="h-7 px-2.5" tone="info" icon={<Loader2 className="animate-spin" />}>
              {t.bg.stillRunning(bgRunning)}
            </Chip>
          )}
          {latest && st.tools > 0 && <Chip className="h-7 px-2.5">{t.conv.tools(st.tools)}</Chip>}
          {latest && st.subagents > 0 && (
            <Chip className="h-7 px-2.5" icon={<Bot />}>
              {t.conv.subagents(st.subagents)}
            </Chip>
          )}
          {latest && (adds > 0 || dels > 0) && (
            <Chip className="h-7 px-2.5">
              <DiffCount adds={adds} dels={dels} />
            </Chip>
          )}
        </div>
      )}
    </div>
  );
}

// ---- background work -------------------------------------------------------------------------------

const BG_EVENT_ICON: Record<BgEventStatus, typeof Check> = { completed: Check, failed: X, stopped: Square, event: Bell };
const BG_EVENT_TONE: Record<BgEventStatus, string> = { completed: 'text-add', failed: 'text-del', stopped: 'text-faint', event: 'text-info' };

/** A background task reported back (a `<task-notification>` line): one compact system row. */
function BgEventRow({ status, summary }: { status: BgEventStatus; summary: string }) {
  const Icon = BG_EVENT_ICON[status];
  return (
    <div className="flex min-w-0 items-start gap-2 text-[13px] text-muted" data-bg-event={status}>
      <Icon className={cx('mt-0.5 size-3.5 shrink-0', BG_EVENT_TONE[status])} aria-label={t.bg.event[status]} />
      <span className="min-w-0 break-words">{summary || t.bg.event[status]}</span>
    </div>
  );
}

const BG_FOLD = 3;

function bgIcon(kind: string): typeof Wrench {
  return kind === 'subagent' || kind === 'teammate' ? Bot : kind === 'shell' ? SquareTerminal : kind === 'monitor' ? Search : ListTree;
}

/** What still runs after the agent's turn: each task with its age; more than 3 fold. */
function BgStrip({ run, onJump }: { run: AgentRun; onJump(toolUseId: string): void }) {
  const tasks = bgTasksOf(run);
  const [all, setAll] = useState(false);
  const now = useNow(1000);
  if (!tasks.length) return null;
  const shown = all || tasks.length <= BG_FOLD ? tasks : tasks.slice(0, BG_FOLD);
  return (
    <div className="mt-2 rounded-lg border border-border bg-surface px-2.5 py-2" data-bg-strip>
      <div className="mb-1 flex items-center gap-2 text-xs text-muted">
        <Loader2 className="size-3.5 animate-spin text-info" />
        <span>{t.bg.title}</span>
      </div>
      {shown.map((b) => (
        <BgRow key={b.id} task={b} now={now} onJump={onJump} />
      ))}
      {tasks.length > BG_FOLD && (
        <button type="button" className="vk-focus mt-1 text-xs text-muted hover:text-fg" aria-expanded={all} onClick={() => setAll(!all)}>
          {all ? t.bg.hide : t.bg.more(tasks.length - BG_FOLD)}
        </button>
      )}
    </div>
  );
}

function BgRow({ task, now, onJump }: { task: BgTask; now: number; onJump(toolUseId: string): void }) {
  const Icon = bgIcon(task.kind);
  const id = task.tool_use_id;
  const body = (
    <>
      <Icon className="size-3.5 shrink-0 text-faint" aria-label={t.bg.kind[task.kind] ?? task.kind} />
      <span className={cx('min-w-0 flex-1 truncate text-xs text-muted', task.kind === 'shell' && 'font-mono')}>{task.label}</span>
      <span className="shrink-0 text-xs tabular-nums text-faint">{shortDuration(now - task.since_ms)}</span>
    </>
  );
  return id ? (
    <button type="button" className={rowCls} title={t.bg.jump} onClick={() => onJump(id)}>
      {body}
    </button>
  ) : (
    <div className={cx(rowCls, 'hover:bg-transparent')}>{body}</div>
  );
}
