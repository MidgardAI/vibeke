// What is selected, derived from the route: the workspace, its tab (pane) and the right panel.
// Per-workspace last tab and the last workspace live in sessionStorage (per window); the panel's
// open state and width are per-device prefs. Also resolves the old routes (Panes, Focus, Changes,
// pane links) to workspaces once the dashboard is known.

import { useMemo } from 'react';
import type { Prefs } from '../lib/prefs';
import { ValueStore, useStore } from '../lib/store';
import { groupWorkspaces, mostUrgent, workspaceOfPane, workspacePinKey, type WorkspaceList, type WorkspaceRow } from '../lib/workspaces';
import { workspaceRoute, type PanelKind, type Route, type WorkspaceRoute } from '../router';
import { usePrefs, useTree } from './hooks';

// ---- layout breakpoints ----------------------------------------------------------------------

/** `wide` ≥ 1100 (sidebar + centre + docked panel), `mid` 960–1099 (panel overlays), `narrow` < 960 (drawer). */
export type LayoutMode = 'wide' | 'mid' | 'narrow';
export const WIDE_MIN = 1100;
export const MID_MIN = 960;

export function layoutModeFor(width: number): LayoutMode {
  return width >= WIDE_MIN ? 'wide' : width >= MID_MIN ? 'mid' : 'narrow';
}

export function currentLayoutMode(): LayoutMode {
  return typeof window === 'undefined' ? 'wide' : layoutModeFor(window.innerWidth);
}

// ---- window-local state ----------------------------------------------------------------------

/** The narrow-layout sidebar drawer. */
export const drawerOpen = new ValueStore(false);

const ss = (): Storage | null => {
  try {
    return typeof sessionStorage === 'undefined' ? null : sessionStorage;
  } catch {
    return null;
  }
};
const TAB_KEY = 'vibeke.tab.';
const LAST_KEY = 'vibeke.lastWorkspace';

export function rememberTab(host: string, ws: string, pane: string): void {
  try {
    ss()?.setItem(`${TAB_KEY}${host}/${ws}`, pane);
    ss()?.setItem(LAST_KEY, JSON.stringify({ host, ws }));
  } catch {
    // storage blocked: selection just isn't remembered
  }
}

export function lastTab(host: string, ws: string): string | null {
  try {
    return ss()?.getItem(`${TAB_KEY}${host}/${ws}`) ?? null;
  } catch {
    return null;
  }
}

export function lastWorkspace(): { host: string; ws: string } | null {
  try {
    const v = JSON.parse(ss()?.getItem(LAST_KEY) ?? 'null') as unknown;
    if (v && typeof v === 'object' && typeof (v as { host?: unknown }).host === 'string' && typeof (v as { ws?: unknown }).ws === 'string') return v as { host: string; ws: string };
  } catch {
    // ignore
  }
  return null;
}

// ---- panel -----------------------------------------------------------------------------------

/**
 * The right panel for a workspace route: an explicit `?panel=` wins; otherwise the per-device
 * default applies on wide windows only (narrower windows would cover the centre with it).
 */
export function effectivePanel(route: WorkspaceRoute, prefs: Pick<Prefs, 'panelOpen'>, mode: LayoutMode): PanelKind | null {
  if (route.panel === 'off') return null;
  if (route.panel) return route.panel;
  return mode === 'wide' && prefs.panelOpen ? 'changes' : null;
}

/**
 * The route after toggling the panel to `kind` (closing it when that tab is already showing), and
 * on wide windows the new per-device default, so the next workspace opens the same way.
 */
export function togglePanelRoute(
  route: WorkspaceRoute,
  prefs: Pick<Prefs, 'panelOpen'>,
  mode: LayoutMode,
  kind: PanelKind = 'changes',
): { route: WorkspaceRoute; panelOpen?: boolean } {
  const open = effectivePanel(route, prefs, mode);
  const base = { ...route, file: null, commit: null };
  if (open === kind) return mode === 'wide' ? { route: { ...base, panel: null }, panelOpen: false } : { route: { ...base, panel: null } };
  if (mode !== 'wide') return { route: { ...base, panel: kind } };
  return { route: { ...base, panel: kind === 'changes' ? null : kind }, panelOpen: true };
}

// ---- legacy routes ---------------------------------------------------------------------------

/**
 * Where an old route goes now, given the workspace rows (most urgent first). `null` = keep the
 * route (not legacy, or the dashboard is not there yet to decide).
 */
export function resolveLegacy(route: Route, rows: readonly WorkspaceRow[], last: { host: string; ws: string } | null = null): Route | null {
  switch (route.name) {
    case 'pane': {
      const ws = workspaceOfPane(rows, route.host, route.pane);
      if (!ws) return null;
      return workspaceRoute(ws.host, ws.workspace.id, { pane: route.pane, panel: route.view === 'changes' ? 'changes' : null, show: route.show ?? null });
    }
    case 'panes':
    case 'focus': {
      const top = mostUrgent([...rows]);
      return top ? workspaceRoute(top.host, top.workspace.id) : { name: 'inbox' };
    }
    case 'changes': {
      const prev = last ? rows.find((r) => r.host === last.host && r.workspace.id === last.ws) : undefined;
      const target = prev ?? mostUrgent([...rows]);
      return target ? workspaceRoute(target.host, target.workspace.id, { panel: 'changes' }) : { name: 'inbox' };
    }
    default:
      return null;
  }
}

// ---- hooks -----------------------------------------------------------------------------------

/** Sidebar data with the user's filters (query is local to the sidebar). */
export function useWorkspaces(query = ''): WorkspaceList {
  const tree = useTree();
  const prefs = usePrefs();
  return useMemo(
    () => groupWorkspaces(tree, { pins: new Set(prefs.pins), query, hostFilter: prefs.hostFilter, showDone: prefs.showDone }),
    [tree, prefs.pins, prefs.hostFilter, prefs.showDone, query],
  );
}

/** Every workspace row (unfiltered, most urgent first). */
export function useWorkspaceRows(): WorkspaceRow[] {
  const tree = useTree();
  const prefs = usePrefs();
  return useMemo(() => groupWorkspaces(tree, { pins: new Set(prefs.pins) }).all, [tree, prefs.pins]);
}

export function useDrawer(): boolean {
  return useStore(drawerOpen);
}

/** The pane a workspace route shows: explicit, else remembered (if still there), else primary. */
export function selectedPane(route: WorkspaceRoute, row: WorkspaceRow | undefined): string | null {
  if (route.pane) return route.pane;
  if (!row) return null;
  const remembered = lastTab(route.host, route.workspace);
  if (remembered && row.panes.some((p) => p.pane.id === remembered)) return remembered;
  return row.primary?.pane.id ?? null;
}

export { workspacePinKey };
