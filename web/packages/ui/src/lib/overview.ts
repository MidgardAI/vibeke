// Overview page model: counts, the workspaces worth a tile, and a feed of what each agent did
// last. Pure functions over the workspace rows (lib/workspaces.ts) and the host list, so the
// desktop and phone layouts read the same numbers as the sidebar. No network and no React here.

import type { AgentRun, HostState } from '@vibeke/core';
import { bgCount } from './bg-tasks';
import { runKey } from './tree';
import type { WorkspaceGroupId, WorkspaceRow } from './workspaces';

/** The overview's own grouping: "finished, not seen yet" counts as ready to review. */
export type OverviewGroup = 'needs' | 'working' | 'review' | 'done' | 'idle';

export interface OverviewCounts {
  needs: number;
  working: number;
  review: number;
  done: number;
  idle: number;
  /** Working rows with background tasks only (no turn in progress). */
  background: number;
}

export interface OverviewHost {
  id: string;
  name: string;
  status: HostState['status'];
  panes: number;
  agents: number;
}

export type FeedKind = 'working' | 'finished' | 'error' | 'rate_limited' | 'exited';

/** One feed entry: the latest state of one agent run. */
export interface FeedEvent {
  /** `<host>/<run>` */
  key: string;
  kind: FeedKind;
  /** When the run entered this state (ms). */
  at: number;
  row: WorkspaceRow;
  pane: string;
  run: AgentRun;
}

export interface Overview {
  counts: OverviewCounts;
  needs: WorkspaceRow[];
  /** "Needs you" rows without an open request (an agent stopped with an error or hit a limit). */
  stopped: WorkspaceRow[];
  working: WorkspaceRow[];
  review: WorkspaceRow[];
  /** Every row that is not idle, most urgent first (the tile grid). */
  active: WorkspaceRow[];
  hosts: OverviewHost[];
  feed: FeedEvent[];
}

export function overviewGroup(r: WorkspaceRow): OverviewGroup {
  const g: WorkspaceGroupId = r.group;
  if (g === 'done' && r.unread) return 'review';
  return g === 'needs' ? 'needs' : g;
}

const FEED_KIND: Partial<Record<AgentRun['execution']['value'], FeedKind>> = {
  starting: 'working',
  working: 'working',
  error: 'error',
  rate_limited: 'rate_limited',
  exited: 'exited',
};

function feedKind(run: AgentRun): FeedKind | null {
  const k = FEED_KIND[run.execution.value];
  if (k) return k;
  // An idle run counts once it did some work; a fresh, untouched agent is not news.
  return run.execution.value === 'idle' && run.turns_completed > 0 ? 'finished' : null;
}

/**
 * Feed entries for `rows`, newest first: the latest run of each pane. Runs come from the hosts'
 * dashboards, not from `PaneRow.run`, so a run that ended (and its pane's row no longer carries)
 * still shows as exited. Without `hosts`, only the rows' current runs count.
 */
export function feedOf(rows: readonly WorkspaceRow[], hosts: readonly HostState[] = []): FeedEvent[] {
  const latest = new Map<string, { row: WorkspaceRow; run: AgentRun }>();
  const consider = (row: WorkspaceRow, run: AgentRun) => {
    const k = `${row.host}/${run.pane}`;
    const cur = latest.get(k);
    if (!cur || run.started_at_ms > cur.run.started_at_ms) latest.set(k, { row, run });
  };
  const rowOfPane = new Map<string, WorkspaceRow>();
  for (const row of rows) {
    for (const p of row.panes) {
      rowOfPane.set(`${row.host}/${p.pane.id}`, row);
      if (p.run) consider(row, p.run);
    }
  }
  for (const h of hosts) {
    for (const run of h.dashboard?.runs ?? []) {
      const row = rowOfPane.get(`${h.record.host_id}/${run.pane}`);
      if (row) consider(row, run);
    }
  }
  const out: FeedEvent[] = [];
  for (const { row, run } of latest.values()) {
    const kind = feedKind(run);
    if (kind) out.push({ key: runKey(row.host, run.id), kind, at: run.execution.since_ms || run.started_at_ms, row, pane: run.pane, run });
  }
  out.sort((a, b) => b.at - a.at || a.key.localeCompare(b.key));
  return out;
}

/** Per-host status for the hosts list. */
export function overviewHosts(hosts: readonly HostState[]): OverviewHost[] {
  return hosts.map((h) => {
    const d = h.dashboard;
    return {
      id: h.record.host_id,
      name: h.info?.host_name ?? h.record.name,
      status: h.status,
      panes: d?.panes.length ?? 0,
      agents: d?.runs.filter((r) => r.execution.value !== 'exited').length ?? 0,
    };
  });
}

/** `rows` must be most urgent first (`groupWorkspaces(...).all`). */
export function buildOverview(rows: readonly WorkspaceRow[], hosts: readonly HostState[]): Overview {
  const counts: OverviewCounts = { needs: 0, working: 0, review: 0, done: 0, idle: 0, background: 0 };
  const needs: WorkspaceRow[] = [];
  const stopped: WorkspaceRow[] = [];
  const working: WorkspaceRow[] = [];
  const review: WorkspaceRow[] = [];
  const active: WorkspaceRow[] = [];
  for (const r of rows) {
    const g = overviewGroup(r);
    counts[g]++;
    if (g === 'needs') {
      needs.push(r);
      if (r.open === 0) stopped.push(r);
    }
    if (g === 'working') {
      working.push(r);
      const turn = r.panes.some((p) => p.run && (p.run.execution.value === 'working' || p.run.execution.value === 'starting'));
      if (!turn && r.panes.some((p) => bgCount(p.run) > 0)) counts.background++;
    }
    if (g === 'review') review.push(r);
    if (g !== 'idle' && g !== 'done') active.push(r);
  }
  // Ready-to-review rows sit between "needs you" and "working" in the tile grid.
  const rank: Record<OverviewGroup, number> = { needs: 0, review: 1, working: 2, done: 3, idle: 4 };
  active.sort((a, b) => rank[overviewGroup(a)] - rank[overviewGroup(b)]);
  return { counts, needs, stopped, working, review, active, hosts: overviewHosts(hosts), feed: feedOf(rows, hosts) };
}

/** The tile grid: at most `limit` tiles; the rest are counted in `more`. */
export function tilesOf(active: readonly WorkspaceRow[], limit: number): { tiles: WorkspaceRow[]; more: number } {
  if (active.length <= limit) return { tiles: [...active], more: 0 };
  // Keep the last slot for the "+N more" tile.
  return { tiles: active.slice(0, limit - 1), more: active.length - (limit - 1) };
}

/** The feed narrowed to one workspace (`<host>/<workspace>`), or all of it. */
export function filterFeed(feed: readonly FeedEvent[], workspaceKey: string | null): FeedEvent[] {
  return workspaceKey ? feed.filter((e) => e.row.key === workspaceKey) : [...feed];
}
