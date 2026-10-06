// Sidebar model: one row per workspace across hosts, grouped by what it asks of the user
// (Needs you → Ready to review → Working → Done → Idle), built on the pane tree (lib/tree.ts) so
// attention, pins and "seen" marks stay the same everywhere.

import { displayName, type Task, type Workspace } from '@vibeke/core';
import type { PaneRow, PaneTree } from './tree';

export type WorkspaceGroupId = 'needs' | 'review' | 'working' | 'done' | 'idle';
export const GROUP_ORDER: readonly WorkspaceGroupId[] = ['needs', 'review', 'working', 'done', 'idle'];

/**
 * An attention entry from the host (`attention.list`, when the gateway passes it through):
 * marks a workspace (or the workspace of a pane) as needing the user or ready for review.
 */
export interface AttentionHint {
  host: string;
  workspace?: string | null;
  pane?: string | null;
  kind: 'needs_you' | 'review';
}

export interface WorkspaceRow {
  /** `<host>/<workspace>` */
  key: string;
  host: string;
  hostName: string;
  workspace: Workspace;
  task: Task | null;
  title: string;
  /** Harness of the primary agent (null: shells only). */
  harness: string | null;
  branch: string | null;
  /** What the primary agent last said or is doing (sub-line when there is no branch). */
  summary: string | null;
  /** Short review/check state for the sub-line (from the task's review label). */
  checkLabel: string | null;
  lastActivityMs: number;
  group: WorkspaceGroupId;
  pinned: boolean;
  /** Finished since the user last looked. */
  unread: boolean;
  /** Open interactions across the workspace's panes. */
  open: number;
  /** The pane the workspace opens on: the one that needs the user, else the latest agent. */
  primary: PaneRow | null;
  panes: PaneRow[];
}

export interface WorkspaceSection {
  id: WorkspaceGroupId;
  rows: WorkspaceRow[];
}

export interface WorkspaceList {
  pinned: WorkspaceRow[];
  groups: WorkspaceSection[];
  /** Every row (unfiltered), most urgent first. */
  all: WorkspaceRow[];
  /** Rows hidden by the filters (for a "N hidden" hint). */
  hidden: number;
}

export interface GroupOptions {
  /** Pinned workspace keys (`ws:<host>/<workspace>`) and pinned pane keys from prefs. */
  pins?: ReadonlySet<string>;
  attention?: readonly AttentionHint[];
  query?: string;
  hostFilter?: string | null;
  showDone?: boolean;
}

export const workspaceKey = (host: string, ws: string): string => `${host}/${ws}`;
export const workspacePinKey = (host: string, ws: string): string => `ws:${host}/${ws}`;

const REVIEW_LABELS = new Set(['ready_for_review', 'review_available']);
const CHECK_LABELS: Record<string, string> = {
  ready_for_review: 'ready for review',
  review_available: 'review available',
  finished_without_review: 'finished',
  needs_task_details: 'needs details',
};

const ACTIVE = new Set(['starting', 'working', 'rate_limited']);

function groupOf(panes: PaneRow[], task: Task | null, hints: readonly AttentionHint[]): WorkspaceGroupId {
  if (hints.some((h) => h.kind === 'needs_you')) return 'needs';
  if (panes.some((r) => r.attention === 'interaction' || r.run?.execution.value === 'error')) return 'needs';
  const label = typeof task?.review_label === 'string' ? task.review_label : null;
  if ((label && REVIEW_LABELS.has(label)) || hints.some((h) => h.kind === 'review')) return 'review';
  if (panes.some((r) => r.run && ACTIVE.has(r.run.execution.value))) return 'working';
  if (panes.some((r) => r.run && r.run.execution.value === 'idle' && r.run.turns_completed > 0)) return 'done';
  return 'idle';
}

const PRIMARY_RANK: Record<PaneRow['attention'], number> = { interaction: 0, needs_input: 1, working: 2, idle: 3 };

/** The pane a workspace opens on. */
export function primaryPane(panes: readonly PaneRow[]): PaneRow | null {
  if (!panes.length) return null;
  const agents = panes.filter((r) => r.run);
  const pool = agents.length ? agents : panes;
  return [...pool].sort(
    (a, b) =>
      PRIMARY_RANK[a.attention] - PRIMARY_RANK[b.attention] ||
      (b.run?.execution.since_ms ?? 0) - (a.run?.execution.since_ms ?? 0) ||
      panes.indexOf(a) - panes.indexOf(b),
  )[0]!;
}

function lastActivity(panes: PaneRow[], task: Task | null): number {
  let t = 0;
  for (const r of panes) {
    if (r.run) t = Math.max(t, r.run.execution.since_ms, r.run.started_at_ms);
    for (const i of r.open) t = Math.max(t, i.opened_at_ms);
  }
  if (!t && task) t = task.created_at_ms;
  return t;
}

/** All workspace rows of a pane tree, unfiltered and unsorted. */
export function workspaceRows(tree: PaneTree, o: GroupOptions = {}): WorkspaceRow[] {
  const pins = o.pins ?? new Set<string>();
  const hints = o.attention ?? [];
  const rows: WorkspaceRow[] = [];
  for (const g of tree.hosts) {
    const host = g.host.record.host_id;
    const hostName = g.host.info?.host_name ?? g.host.record.name;
    const d = g.host.dashboard;
    if (!d) continue;
    for (const w of g.workspaces) {
      const panes = w.tabs.flatMap((tg) => tg.rows);
      const task = d.tasks.find((x) => x.workspace === w.workspace.id || (w.workspace.task !== null && x.id === w.workspace.task)) ?? null;
      const paneIds = new Set(panes.map((r) => r.pane.id));
      const mine = hints.filter((h) => h.host === host && (h.workspace === w.workspace.id || (!!h.pane && paneIds.has(h.pane))));
      const primary = primaryPane(panes);
      const label = typeof task?.review_label === 'string' ? task.review_label : null;
      const group = groupOf(panes, task, mine);
      rows.push({
        key: workspaceKey(host, w.workspace.id),
        host,
        hostName,
        workspace: w.workspace,
        task,
        title: task?.title || displayName(w.workspace),
        harness: primary?.run?.harness ?? null,
        branch: w.workspace.branch ?? task?.branch ?? null,
        summary: primary?.run?.last_message?.split('\n')[0]?.trim() || primary?.run?.task || null,
        checkLabel: label ? (CHECK_LABELS[label] ?? null) : null,
        lastActivityMs: lastActivity(panes, task),
        group,
        pinned: pins.has(workspacePinKey(host, w.workspace.id)) || panes.some((r) => r.pinned),
        // "Finished, look": runAttention reports an unseen finished run as needs_input on an idle run.
        unread: panes.some((r) => r.attention === 'needs_input' && r.run?.execution.value === 'idle'),
        open: panes.reduce((n, r) => n + r.open.length, 0),
        primary,
        panes,
      });
    }
  }
  return rows;
}

const GROUP_RANK = Object.fromEntries(GROUP_ORDER.map((g, i) => [g, i])) as Record<WorkspaceGroupId, number>;

function compareRows(a: WorkspaceRow, b: WorkspaceRow): number {
  if (a.group !== b.group) return GROUP_RANK[a.group] - GROUP_RANK[b.group];
  if (a.group === 'needs') {
    // Longest-waiting first, like the inbox.
    const wa = Math.min(...a.panes.flatMap((r) => r.open.map((i) => i.opened_at_ms)), Number.MAX_SAFE_INTEGER);
    const wb = Math.min(...b.panes.flatMap((r) => r.open.map((i) => i.opened_at_ms)), Number.MAX_SAFE_INTEGER);
    if (wa !== wb) return wa - wb;
  }
  return b.lastActivityMs - a.lastActivityMs || a.title.localeCompare(b.title) || a.key.localeCompare(b.key);
}

function matches(r: WorkspaceRow, q: string): boolean {
  if (!q) return true;
  const hay = `${r.title} ${displayName(r.workspace)} ${r.branch ?? ''} ${r.hostName} ${r.harness ?? ''}`.toLowerCase();
  return q
    .toLowerCase()
    .split(/\s+/)
    .filter(Boolean)
    .every((w) => hay.includes(w));
}

/** Sidebar sections: pinned rows, then non-empty status groups (filters applied). */
export function groupWorkspaces(tree: PaneTree, o: GroupOptions = {}): WorkspaceList {
  const all = workspaceRows(tree, o).sort(compareRows);
  const q = (o.query ?? '').trim();
  const showDone = o.showDone ?? true;
  const visible = all.filter(
    (r) => matches(r, q) && (!o.hostFilter || r.host === o.hostFilter) && (showDone || r.pinned || (r.group !== 'done' && r.group !== 'idle')),
  );
  // Pinned rows sit in their own section, except when they need the user: then they also stay
  // at the top of "Needs you" so nothing urgent hides in a collapsed section.
  const pinned = visible.filter((r) => r.pinned);
  const groups: WorkspaceSection[] = [];
  for (const id of GROUP_ORDER) {
    const rows = visible.filter((r) => r.group === id && (!r.pinned || id === 'needs'));
    if (rows.length) groups.push({ id, rows });
  }
  return { pinned, groups, all, hidden: all.length - visible.length };
}

/** The workspace that most needs attention (redirect target of the old Panes/Focus tabs). */
export function mostUrgent(list: WorkspaceList | WorkspaceRow[]): WorkspaceRow | null {
  const rows = Array.isArray(list) ? list : list.all;
  return rows[0] ?? null;
}

/** The workspace row containing a pane. */
export function workspaceOfPane(rows: readonly WorkspaceRow[], host: string, pane: string): WorkspaceRow | null {
  return rows.find((r) => r.host === host && r.panes.some((p) => p.pane.id === pane)) ?? null;
}
