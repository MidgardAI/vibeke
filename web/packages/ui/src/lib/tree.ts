// Panes tab model (spec 16 §9.1 Home/Space): hosts → workspaces → tabs → panes, with what each
// pane asks of the user, per-device pins, and the "N need you" summary.

import {
  displayName,
  runAttention,
  type AgentRun,
  type Attention,
  type Dashboard,
  type HostState,
  type Interaction,
  type LayoutNode,
  type Pane,
  type Tab,
  type Workspace,
} from '@vibeke/core';

export interface PaneRow {
  key: string;
  host: string;
  hostName: string;
  pane: Pane;
  tab: Tab | undefined;
  workspace: Workspace | undefined;
  run: AgentRun | null;
  open: Interaction[];
  attention: Attention;
  needsYou: boolean;
  pinned: boolean;
}

export interface TabGroup {
  tab: Tab;
  rows: PaneRow[];
}

export interface WorkspaceGroup {
  workspace: Workspace;
  tabs: TabGroup[];
  needsYou: number;
}

export interface HostGroup {
  host: HostState;
  workspaces: WorkspaceGroup[];
  rows: PaneRow[];
  needsYou: number;
}

export interface PaneTree {
  hosts: HostGroup[];
  pinned: PaneRow[];
  needYou: PaneRow[];
  all: PaneRow[];
}

export const paneKey = (host: string, pane: string): string => `${host}/${pane}`;
export const runKey = (host: string, run: string): string => `${host}/${run}`;

/** Leaf panes of a layout in reading order. */
export function layoutOrder(node: LayoutNode | undefined): string[] {
  if (!node) return [];
  if ('Leaf' in node) return [node.Leaf.pane];
  return node.Split.children.flatMap(([child]) => layoutOrder(child));
}

/** The live run of a pane (latest started, not ended). */
export function runForPane(d: Dashboard, pane: string): AgentRun | null {
  let best: AgentRun | null = null;
  for (const r of d.runs) {
    if (r.pane !== pane || r.ended_at_ms !== null) continue;
    if (!best || r.started_at_ms > best.started_at_ms) best = r;
  }
  return best;
}

export interface TreeOptions {
  pins: ReadonlySet<string>;
  /** done_rev the user has seen per run key; a higher done_rev means "finished, look". */
  seenDone: Readonly<Record<string, number>>;
}

export function paneRows(h: HostState, o: TreeOptions): PaneRow[] {
  const d = h.dashboard;
  if (!d) return [];
  const host = h.record.host_id;
  const hostName = h.info?.host_name ?? h.record.name;
  const open = d.interactions.filter((i) => i.status === 'open');
  const tabs = new Map(d.tabs.map((t) => [t.id, t]));
  const wss = new Map(d.workspaces.map((w) => [w.id, w]));
  return d.panes.map((pane) => {
    const run = runForPane(d, pane.id);
    const mine = open.filter((i) => i.pane === pane.id);
    let attention: Attention = 'idle';
    if (mine.length) attention = 'interaction';
    else if (run) attention = runAttention(run, open, { seenDoneRev: (r) => o.seenDone[runKey(host, r.id)] });
    const key = paneKey(host, pane.id);
    return {
      key,
      host,
      hostName,
      pane,
      tab: tabs.get(pane.tab),
      workspace: wss.get(pane.workspace),
      run,
      open: mine,
      attention,
      needsYou: attention === 'interaction' || attention === 'needs_input',
      pinned: o.pins.has(key),
    };
  });
}

const ATT_RANK: Record<Attention, number> = { interaction: 0, needs_input: 1, working: 2, idle: 3 };

export function buildTree(hosts: readonly HostState[], o: TreeOptions): PaneTree {
  const groups: HostGroup[] = [];
  const all: PaneRow[] = [];
  for (const h of hosts) {
    const rows = paneRows(h, o);
    all.push(...rows);
    const d = h.dashboard;
    const workspaces: WorkspaceGroup[] = [];
    if (d) {
      const byTab = new Map<string, PaneRow[]>();
      for (const r of rows) {
        const list = byTab.get(r.pane.tab) ?? [];
        list.push(r);
        byTab.set(r.pane.tab, list);
      }
      for (const ws of [...d.workspaces].sort((a, b) => a.order - b.order)) {
        const tabs: TabGroup[] = [];
        for (const tab of d.tabs.filter((t) => t.workspace === ws.id).sort((a, b) => a.order - b.order || a.number - b.number)) {
          const order = layoutOrder(tab.layout);
          const list = (byTab.get(tab.id) ?? []).sort(
            (a, b) => (order.indexOf(a.pane.id) + 1 || 999) - (order.indexOf(b.pane.id) + 1 || 999),
          );
          if (list.length) tabs.push({ tab, rows: list });
        }
        workspaces.push({ workspace: ws, tabs, needsYou: tabs.reduce((n, t) => n + t.rows.filter((r) => r.needsYou).length, 0) });
      }
    }
    groups.push({ host: h, workspaces, rows, needsYou: rows.filter((r) => r.needsYou).length });
  }
  const needYou = all
    .filter((r) => r.needsYou)
    .sort((a, b) => ATT_RANK[a.attention] - ATT_RANK[b.attention] || (a.open[0]?.opened_at_ms ?? 0) - (b.open[0]?.opened_at_ms ?? 0));
  return { hosts: groups, pinned: all.filter((r) => r.pinned), needYou, all };
}

export const workspaceLabel = (w: Workspace | undefined): string => (w ? displayName(w) : '');

/** One-line label of a pane row: workspace · tab title / pane title. */
export function rowTitle(r: PaneRow): string {
  return r.pane.title ?? r.run?.name ?? r.run?.title ?? r.pane.auto_title;
}

/** Previous/next pane keys on the same host in tree order (pane view prev/next). */
export function neighbours(tree: PaneTree, host: string, pane: string): { prev: PaneRow | null; next: PaneRow | null } {
  const h = tree.hosts.find((g) => g.host.record.host_id === host);
  const ordered = h ? h.workspaces.flatMap((w) => w.tabs.flatMap((t) => t.rows)) : [];
  const i = ordered.findIndex((r) => r.pane.id === pane);
  return { prev: i > 0 ? ordered[i - 1]! : null, next: i >= 0 && i + 1 < ordered.length ? ordered[i + 1]! : null };
}
