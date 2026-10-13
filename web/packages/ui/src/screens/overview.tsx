// Overview (`#/overview`, where `#/` lands): every host's workspaces at a glance. Wide windows get
// count tiles, the requests that need the user (answered in place), the agents at work, what is
// ready to review, previews, an activity list and the hosts. Phones get a grid of workspace tiles
// over one feed: tapping a tile narrows the feed to that workspace, and the grid shrinks to a strip
// once the feed scrolls under it. Answers happen in the feed only, never on a tile.

import { useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { Check, Globe, Plus, X } from 'lucide-react';
import type { InboxItem } from '@vibeke/core';
import { useApprovalCount } from '../app/approval-stores';
import { useHosts, useInboxItems, useNow } from '../app/hooks';
import { emitUi } from '../app/keyboard';
import { useWorkspaceRows } from '../app/selection';
import { TopBar, useMediaQuery } from '../app/shell';
import { Button, DiffCount, HarnessIcon, Kbd, RelTime, StatusDot, cx, type Status } from '../components/ui';
import { t } from '../i18n';
import { bgCount } from '../lib/bg-tasks';
import { excerpt, sumChanges } from '../lib/catch-up';
import { relTime, shortDuration } from '../lib/format';
import { buildOverview, filterFeed, overviewGroup, tilesOf, type FeedEvent, type FeedKind, type Overview, type OverviewHost } from '../lib/overview';
import { workspacePreviews } from '../lib/previews';
import { useScrollMemory } from '../lib/scroll-memory';
import { toolSummary } from '../lib/tool-summary';
import { useGitStatus } from '../lib/use-git-status';
import type { WorkspaceRow } from '../lib/workspaces';
import { navigate, workspaceRoute } from '../router';
import { InboxScreen, type InboxFilter } from './inbox';

const FEED_STATUS: Record<FeedKind, Status> = { working: 'working', finished: 'review', error: 'error', rate_limited: 'need', exited: 'idle' };
const FEED_TONE: Record<FeedKind, string> = { working: 'text-info', finished: 'text-add', error: 'text-del', rate_limited: 'text-need', exited: 'text-muted' };

function useOverview(): Overview {
  const rows = useWorkspaceRows();
  const hosts = useHosts();
  return useMemo(() => buildOverview(rows, hosts), [rows, hosts]);
}

const openRow = (r: WorkspaceRow, pane?: string | null) => navigate(workspaceRoute(r.host, r.workspace.id, pane ? { pane } : {}));

/** The tool the agent runs now, as a short label (`Edit`, `Shell`). */
function toolLabel(r: WorkspaceRow): string | null {
  const run = r.primary?.run;
  return run?.last_tool ? toolSummary(run.last_tool, null, run.cwd).label : null;
}

/** When the oldest open request of `items` was opened (ms), or 0. */
const oldestOpen = (items: readonly InboxItem[]): number => items.reduce((m, it) => (m && m < it.interaction.opened_at_ms ? m : it.interaction.opened_at_ms), 0);

export function OverviewScreen() {
  const wide = useMediaQuery('(min-width: 960px)');
  const main = useRef<HTMLElement>(null);
  useScrollMemory('overview', main);
  return (
    <div className="flex h-full min-h-0 flex-col pt-safe px-safe">
      <TopBar
        title={t.overview.title}
        right={
          <Button size="sm" variant={wide ? 'primary' : 'ghost'} icon={<Plus />} aria-label={t.sidebar.newAgent} onClick={() => emitUi('new-agent')}>
            {wide ? t.sidebar.newAgent : null}
          </Button>
        }
      />
      <main ref={main} className="vk-scroll min-h-0 flex-1 overflow-y-auto">
        {wide ? <Desktop /> : <Phone />}
      </main>
    </div>
  );
}

// ---- desktop ----------------------------------------------------------------------------------

function Desktop() {
  const ov = useOverview();
  const items = useInboxItems();
  const approvals = useApprovalCount();
  const now = useNow(30_000);
  const multiHost = ov.hosts.length > 1;
  const needs = items.length + approvals + ov.stopped.length;
  const oldest = oldestOpen(items);
  const online = ov.hosts.filter((h) => h.status === 'online').length;
  const scrollTo = (id: string) => document.getElementById(id)?.scrollIntoView({ behavior: 'smooth', block: 'start' });

  return (
    <div className="mx-auto w-full max-w-[1280px] space-y-5 px-6 pb-10 pt-5">
      <header>
        <h2 className="text-xl font-semibold tracking-tight">{t.overview.greeting(new Date(now).getHours())}</h2>
        <p className="text-sm text-muted">{t.overview.headline(needs, ov.counts.working, ov.counts.review)}</p>
      </header>

      <div className="grid grid-cols-5 gap-2.5">
        <StatTile label={t.overview.needs} value={needs} tone={needs ? 'need' : undefined} sub={oldest ? t.overview.oldest(shortDuration(now - oldest)) : undefined} onClick={() => scrollTo('ov-needs')} />
        <StatTile label={t.overview.working} value={ov.counts.working} tone={ov.counts.working ? 'info' : undefined} sub={ov.counts.background ? t.overview.background(ov.counts.background) : undefined} onClick={() => scrollTo('ov-working')} />
        <StatTile label={t.overview.review} value={ov.counts.review} tone={ov.counts.review ? 'add' : undefined} onClick={() => scrollTo('ov-review')} />
        <StatTile label={t.overview.done} value={ov.counts.done} sub={ov.counts.idle ? t.overview.idle(ov.counts.idle) : undefined} />
        <StatTile label={t.overview.hosts} value={ov.hosts.length} sub={t.overview.hostsOnline(online, ov.hosts.length)} onClick={() => scrollTo('ov-hosts')} />
      </div>

      <div className="grid grid-cols-1 gap-5 xl:grid-cols-[minmax(0,1.35fr)_minmax(0,1fr)]">
        <div className="min-w-0 space-y-5">
          <Section id="ov-needs" title={t.overview.needs} tone="need" right={needs > 0 && <KeyHints />}>
            {ov.stopped.length > 0 && (
              <List>
                {ov.stopped.map((r) => (
                  <StoppedRow key={r.key} row={r} showHost={multiHost} />
                ))}
              </List>
            )}
            <div className="-mx-3 -mb-6 -mt-2 sm:-mx-4">
              <InboxScreen
                empty={
                  ov.stopped.length ? null : (
                    <div className="px-3 pb-6 pt-2 sm:px-4">
                      <Quiet title={t.overview.calm} hint={t.overview.calmHint} />
                    </div>
                  )
                }
              />
            </div>
          </Section>
          <Section id="ov-working" title={t.overview.workingNow} count={ov.working.length}>
            {ov.working.length ? (
              <List>
                {ov.working.map((r) => (
                  <WorkingRow key={r.key} row={r} now={now} showHost={multiHost} />
                ))}
              </List>
            ) : (
              <Quiet title={t.overview.noWorking} />
            )}
          </Section>
        </div>

        <div className="min-w-0 space-y-5">
          <Section id="ov-review" title={t.overview.review} count={ov.review.length}>
            {ov.review.length ? (
              <div className="space-y-2">
                {ov.review.map((r) => (
                  <ReviewCard key={r.key} row={r} showHost={multiHost} />
                ))}
              </div>
            ) : (
              <Quiet title={t.overview.noReview} />
            )}
          </Section>
          <Previews />
          <Section title={t.overview.activity}>
            {ov.feed.length ? (
              <List>
                {ov.feed.slice(0, 12).map((e) => (
                  <ActivityRow key={e.key} event={e} now={now} />
                ))}
              </List>
            ) : (
              <Quiet title={t.overview.noActivity} />
            )}
          </Section>
          <Section id="ov-hosts" title={t.overview.hosts}>
            <List>
              {ov.hosts.map((h) => (
                <HostRow key={h.id} host={h} />
              ))}
            </List>
          </Section>
        </div>
      </div>
    </div>
  );
}

function StatTile({ label, value, sub, tone, onClick }: { label: string; value: number; sub?: string; tone?: 'need' | 'info' | 'add'; onClick?(): void }) {
  const toneText = tone === 'need' ? 'text-need' : tone === 'info' ? 'text-info' : tone === 'add' ? 'text-add' : 'text-fg';
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={!onClick}
      className={cx(
        'vk-focus flex min-w-0 flex-col items-start rounded-xl border bg-surface px-3.5 py-3 text-left transition-colors enabled:hover:bg-hover disabled:cursor-default',
        tone === 'need' ? 'border-need/50' : 'border-border',
      )}
    >
      <span className={cx('text-[11px] font-semibold uppercase tracking-wider', tone === 'need' ? 'text-need' : 'text-muted')}>{label}</span>
      <span className={cx('text-2xl font-semibold tabular-nums', toneText)}>{value}</span>
      <span className="h-4 truncate text-xs text-faint">{sub}</span>
    </button>
  );
}

function Section({ id, title, count, tone, right, children }: { id?: string; title: string; count?: number; tone?: 'need'; right?: ReactNode; children: ReactNode }) {
  return (
    <section id={id} aria-label={title} className="scroll-mt-4 space-y-2">
      <div className="flex min-h-6 items-center gap-2">
        <h3 className={cx('text-[11px] font-semibold uppercase tracking-wider', tone === 'need' ? 'text-need' : 'text-muted')}>{title}</h3>
        {!!count && <span className="text-xs tabular-nums text-faint">{count}</span>}
        <span className="flex-1" />
        {right}
      </div>
      {children}
    </section>
  );
}

const List = ({ children }: { children: ReactNode }) => <div className="divide-y divide-border overflow-hidden rounded-xl border border-border bg-surface">{children}</div>;

function Quiet({ title, hint }: { title: string; hint?: string }) {
  return (
    <div className="flex items-center gap-2.5 rounded-xl border border-dashed border-border px-3.5 py-3 text-sm text-muted">
      <Check className="size-4 shrink-0 text-faint" />
      <span className="min-w-0">
        {title}
        {hint && <span className="block text-xs text-faint">{hint}</span>}
      </span>
    </div>
  );
}

function KeyHints() {
  return (
    <span className="hidden items-center gap-1 text-xs text-faint pointer-fine:flex">
      <Kbd>a</Kbd> {t.overview.keyAllow} <Kbd>d</Kbd> {t.overview.keyDeny} <Kbd>j</Kbd>
      <Kbd>k</Kbd> {t.overview.keyMove}
    </span>
  );
}

function WorkingRow({ row, now, showHost }: { row: WorkspaceRow; now: number; showHost: boolean }) {
  const run = row.primary?.run;
  const active = row.panes.some((p) => p.run && (p.run.execution.value === 'working' || p.run.execution.value === 'starting'));
  const tool = toolLabel(row);
  const message = excerpt(run?.last_message, 160);
  const bg = bgCount(run);
  const meta = [showHost ? row.hostName : null, run ? shortDuration(now - (run.execution.since_ms || run.started_at_ms)) : null, run?.turns_completed ? t.overview.turns(run.turns_completed) : null].filter(Boolean);
  return (
    <button type="button" onClick={() => openRow(row, row.primary?.pane.id)} className="vk-focus flex w-full min-w-0 flex-col gap-0.5 px-3.5 py-2.5 text-left hover:bg-hover">
      <span className="flex w-full min-w-0 items-center gap-2">
        <StatusDot status={active ? 'working' : 'background'} />
        <HarnessIcon harness={row.harness} />
        <span className="truncate text-sm font-medium">{row.title}</span>
        <span className="truncate text-xs text-faint">{meta.join(' · ')}</span>
      </span>
      <span className="flex w-full min-w-0 items-center gap-1.5 pl-[30px] text-xs text-muted">
        {tool && <span className="shrink-0 text-fg/80">{tool}</span>}
        {tool && message && <span className="text-faint">·</span>}
        {message && <span className="truncate">{message}</span>}
        {!tool && !message && bg > 0 && <span>{t.bg.count(bg)}</span>}
      </span>
    </button>
  );
}

/** A workspace that needs the user without a request: its agent stopped with an error. */
function StoppedRow({ row, showHost }: { row: WorkspaceRow; showHost: boolean }) {
  const run = row.panes.find((p) => p.run?.execution.value === 'error')?.run ?? row.primary?.run;
  const message = excerpt(run?.execution.detail || run?.last_message, 160);
  return (
    <button type="button" onClick={() => openRow(row, run?.pane)} className="vk-focus flex w-full min-w-0 flex-col gap-0.5 px-3.5 py-2.5 text-left hover:bg-hover">
      <span className="flex w-full min-w-0 items-center gap-2">
        <StatusDot status="error" />
        <HarnessIcon harness={row.harness} />
        <span className="truncate text-sm font-medium">{row.title}</span>
        {showHost && <span className="truncate text-xs text-faint">{row.hostName}</span>}
        <span className="flex-1" />
        <span className="shrink-0 text-xs text-del">{t.overview.event.error}</span>
      </span>
      {message && <span className="truncate pl-[30px] text-xs text-muted">{message}</span>}
    </button>
  );
}

function ReviewCard({ row, showHost }: { row: WorkspaceRow; showHost: boolean }) {
  const pane = row.primary?.pane.id ?? null;
  const git = useGitStatus(row.host, pane, { poll: false });
  const totals = git.status ? sumChanges(git.status.files) : null;
  const meta = [showHost ? row.hostName : null, row.branch, row.checkLabel].filter(Boolean);
  const message = excerpt(row.primary?.run?.last_message, 140);
  return (
    <div className="space-y-1.5 rounded-xl border border-border bg-surface px-3.5 py-3">
      <div className="flex min-w-0 items-center gap-2">
        <StatusDot status="review" />
        <HarnessIcon harness={row.harness} />
        <span className="max-w-[60%] shrink-0 truncate text-sm font-medium">{row.title}</span>
        <span className="min-w-0 flex-1 truncate text-xs text-faint">{meta.join(' · ')}</span>
        {totals && <DiffCount adds={totals.adds} dels={totals.dels} />}
      </div>
      {message && <p className="line-clamp-2 text-sm text-muted">{message}</p>}
      <div className="flex gap-2 pt-0.5">
        <Button size="sm" variant="primary" onClick={() => navigate(workspaceRoute(row.host, row.workspace.id, { pane, panel: 'changes' }))}>
          {t.overview.reviewDiff}
        </Button>
        <Button size="sm" onClick={() => openRow(row, pane)}>
          {t.overview.openWorkspace}
        </Button>
      </div>
    </div>
  );
}

function Previews() {
  const hosts = useHosts();
  const rows = useWorkspaceRows();
  const list = useMemo(() => {
    const out: { key: string; label: string; up: boolean; row: WorkspaceRow; id: string }[] = [];
    for (const h of hosts) {
      const hostRows = rows.filter((r) => r.host === h.record.host_id);
      for (const row of hostRows) {
        for (const p of workspacePreviews(row, h.dashboard?.previews, hostRows)) {
          if (p.status === 'suggested') continue;
          out.push({ key: `${h.record.host_id}/${p.id}`, label: p.label || `:${p.port}`, up: p.status !== 'down', row, id: p.id });
        }
      }
    }
    return out;
  }, [hosts, rows]);
  if (!list.length) return null;
  return (
    <Section title={t.overview.previews} count={list.length}>
      <List>
        {list.map((p) => (
          <button
            key={p.key}
            type="button"
            onClick={() => navigate(workspaceRoute(p.row.host, p.row.workspace.id, { panel: 'previews', preview: p.id }))}
            className="vk-focus flex w-full min-w-0 items-center gap-2 px-3.5 py-2 text-left text-sm hover:bg-hover"
          >
            <Globe className="size-3.5 shrink-0 text-muted" />
            <span className="truncate font-medium">{p.label}</span>
            <span className="truncate text-xs text-faint">{p.row.title}</span>
            <span className="flex-1" />
            <StatusDot status={p.up ? 'review' : 'offline'} label={p.up ? undefined : t.sidebar.offline} />
          </button>
        ))}
      </List>
    </Section>
  );
}

function ActivityRow({ event, now }: { event: FeedEvent; now: number }) {
  const message = excerpt(event.run.last_message, 120);
  return (
    <button type="button" onClick={() => openRow(event.row, event.pane)} className="vk-focus grid w-full grid-cols-[40px_10px_minmax(0,1fr)] items-baseline gap-x-2 px-3.5 py-2 text-left hover:bg-hover">
      <span className="font-mono text-[11px] tabular-nums text-faint">{relTime(event.at, now)}</span>
      <StatusDot status={FEED_STATUS[event.kind]} className="translate-y-[-1px]" />
      <span className="min-w-0 truncate text-sm">
        <span className="font-medium">{event.row.title}</span> <span className={FEED_TONE[event.kind]}>{t.overview.event[event.kind]}</span>
        {message && <span className="text-muted"> · {message}</span>}
      </span>
    </button>
  );
}

function HostRow({ host }: { host: OverviewHost }) {
  const online = host.status === 'online';
  return (
    <button type="button" onClick={() => navigate({ name: 'crew' })} className="vk-focus flex w-full min-w-0 items-center gap-2 px-3.5 py-2 text-left text-sm hover:bg-hover">
      <StatusDot status={online ? 'review' : host.status === 'connecting' ? 'background' : 'offline'} />
      <span className="truncate font-medium">{host.name}</span>
      <span className="flex-1" />
      <span className="shrink-0 text-xs text-faint">{online ? t.overview.panes(host.panes) : (t.overview.hostStatus[host.status] ?? host.status)}</span>
    </button>
  );
}

// ---- phone ------------------------------------------------------------------------------------

const TILE_LIMIT = 6;

const TILE_TONE: Record<string, string> = {
  needs: 'border-need/60 bg-need/15',
  review: 'border-add/30 bg-add/10',
  working: 'border-info/25 bg-info/10',
};

function Phone() {
  const ov = useOverview();
  const needs = useInboxItems().length + useApprovalCount() + ov.stopped.length;
  const now = useNow(30_000);
  const [selected, setSelected] = useState<string | null>(null);
  // Forget a filter whose workspace went away.
  const sel = selected && ov.active.some((r) => r.key === selected) ? selected : null;
  const selRow = sel ? ov.active.find((r) => r.key === sel)! : null;
  const { tiles, more } = tilesOf(ov.active, TILE_LIMIT);
  const quiet = ov.counts.done + ov.counts.idle;
  const toggle = (key: string) => setSelected((s) => (s === key ? null : key));

  // The grid shrinks to a strip once it scrolls out of view.
  const grid = useRef<HTMLDivElement>(null);
  const [strip, setStrip] = useState(false);
  useEffect(() => {
    const el = grid.current;
    if (!el || typeof IntersectionObserver === 'undefined') return;
    const io = new IntersectionObserver(([e]) => setStrip(!!e && !e.isIntersecting), { threshold: 0 });
    io.observe(el);
    return () => io.disconnect();
  }, [ov.active.length > 0]);

  const filter = useMemo<InboxFilter | undefined>(
    () =>
      selRow
        ? {
            item: (it) => it.host_id === selRow.host && selRow.panes.some((p) => p.pane.id === it.interaction.pane),
            approval: (host, r) => host === selRow.host && r.workspace === selRow.workspace.id,
          }
        : undefined,
    [selRow],
  );
  const feed = filterFeed(ov.feed, sel).slice(0, 40);

  return (
    <div className="pb-8">
      {strip && ov.active.length > 0 && (
        <div className="sticky top-0 z-10 flex gap-1.5 overflow-x-auto border-b border-border bg-bg/95 px-3 py-2 backdrop-blur [scrollbar-width:none]">
          {ov.active.map((r) => (
            <button
              key={r.key}
              type="button"
              aria-pressed={r.key === sel}
              aria-label={t.overview.filterBy(r.title)}
              onClick={() => toggle(r.key)}
              className={cx('vk-focus flex shrink-0 items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs', r.key === sel ? 'border-fg text-fg' : 'border-border text-muted')}
            >
              <StatusDot status={tileStatus(r)} />
              <span className="max-w-[110px] truncate">{r.title}</span>
            </button>
          ))}
        </div>
      )}

      <div className="space-y-3 px-3 pt-3">
        <p className="px-1 text-sm text-muted">{t.overview.headline(needs, ov.counts.working, ov.counts.review)}</p>
        {ov.active.length > 0 && (
          <div ref={grid} className="grid grid-cols-3 gap-1.5">
            {tiles.map((r) => (
              <Tile key={r.key} row={r} now={now} selected={r.key === sel} onClick={() => toggle(r.key)} />
            ))}
            {more > 0 && (
              <button type="button" onClick={() => emitUi('sidebar')} className="vk-focus flex min-h-[64px] items-center justify-center rounded-xl border border-border bg-surface text-sm text-muted">
                {t.overview.more(more)}
              </button>
            )}
          </div>
        )}
        {(selRow || quiet > 0) && (
          <div className="flex min-h-7 items-center gap-2 px-1">
            {selRow && (
              <button type="button" onClick={() => setSelected(null)} aria-label={t.overview.clearFilter} className="vk-focus flex min-w-0 items-center gap-1 rounded-full border border-fg px-2.5 py-0.5 text-xs">
                <span className="truncate">{selRow.title}</span>
                <X className="size-3 shrink-0" />
              </button>
            )}
            <span className="flex-1" />
            {selRow ? (
              <Button size="sm" variant="ghost" onClick={() => openRow(selRow, selRow.primary?.pane.id)}>
                {t.overview.openWorkspace}
              </Button>
            ) : (
              <button type="button" onClick={() => emitUi('sidebar')} className="text-xs text-faint">
                {t.overview.idle(quiet)}
              </button>
            )}
          </div>
        )}
      </div>

      <div className="-mt-1">
        <InboxScreen key={sel ?? 'all'} filter={filter} empty={null} />
      </div>

      <div className="space-y-3 px-3">
        {feed.length > 0 && <h3 className="px-1 pt-1 text-[11px] font-semibold uppercase tracking-wider text-muted">{t.overview.earlier}</h3>}
        {feed.map((e) => (
          <FeedBubble key={e.key} event={e} now={now} />
        ))}
        {!feed.length && needs === 0 && <Quiet title={ov.active.length ? t.overview.calm : t.overview.noActivity} />}
      </div>
    </div>
  );
}

function tileStatus(r: WorkspaceRow): Status {
  const g = overviewGroup(r);
  if (g === 'needs') return r.panes.some((p) => p.run?.execution.value === 'error') && !r.open ? 'error' : 'need';
  if (g === 'review') return 'review';
  return r.panes.some((p) => p.run && (p.run.execution.value === 'working' || p.run.execution.value === 'starting')) ? 'working' : 'background';
}

function Tile({ row, now, selected, onClick }: { row: WorkspaceRow; now: number; selected: boolean; onClick(): void }) {
  const g = overviewGroup(row);
  const status = tileStatus(row);
  const oldest = row.panes.flatMap((p) => p.open.map((i) => i.opened_at_ms)).reduce((m, x) => (m && m < x ? m : x), 0);
  const label =
    g === 'needs' ? (status === 'error' ? t.overview.event.error : t.overview.needs) : g === 'review' ? t.overview.review : (toolLabel(row) ?? t.overview.working);
  return (
    <button
      type="button"
      aria-pressed={selected}
      aria-label={t.overview.filterBy(row.title)}
      onClick={onClick}
      className={cx('vk-focus flex min-h-[64px] min-w-0 flex-col gap-0.5 rounded-xl border px-2.5 py-2 text-left', TILE_TONE[g] ?? 'border-border bg-surface', selected && 'outline outline-2 outline-offset-1 outline-fg')}
    >
      <span className="flex w-full items-center gap-1">
        <HarnessIcon harness={row.harness} />
        <span className="flex-1" />
        {g === 'needs' && oldest > 0 ? <span className="text-[11px] tabular-nums text-need">{shortDuration(now - oldest)}</span> : <StatusDot status={status} />}
      </span>
      <span className="w-full truncate text-[13px] font-medium">{row.title}</span>
      <span className={cx('w-full truncate text-[11px] font-medium', g === 'needs' ? (status === 'error' ? 'text-del' : 'text-need') : g === 'review' ? 'text-add' : 'text-info')}>{label}</span>
    </button>
  );
}

function FeedBubble({ event, now }: { event: FeedEvent; now: number }) {
  const message = excerpt(event.run.last_message, 220);
  const tool = event.kind === 'working' && event.run.last_tool ? toolSummary(event.run.last_tool, null, event.run.cwd).label : null;
  return (
    <button type="button" onClick={() => openRow(event.row, event.pane)} className="vk-focus grid w-full grid-cols-[24px_minmax(0,1fr)] gap-x-2.5 gap-y-1 text-left">
      <span className="row-span-2 flex size-6 items-center justify-center rounded-md bg-surface-2">
        <HarnessIcon harness={event.run.harness} />
      </span>
      <span className="flex min-w-0 items-baseline gap-1.5">
        <span className="truncate text-sm font-medium">{event.row.title}</span>
        <RelTime ms={event.at} now={now} />
      </span>
      <span className="block space-y-1 rounded-[4px_14px_14px_14px] border border-border bg-surface px-3 py-2 text-sm">
        <span className={cx('flex items-center gap-1.5 text-xs font-medium', FEED_TONE[event.kind])}>
          <StatusDot status={FEED_STATUS[event.kind]} />
          {t.overview.event[event.kind]}
          {tool && <span className="text-muted">· {tool}</span>}
        </span>
        {message && <span className="line-clamp-3 block text-fg/90">{message}</span>}
      </span>
    </button>
  );
}

