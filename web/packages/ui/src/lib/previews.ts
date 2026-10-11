// The workspace's dev-server previews (spec 06 B3.5): which ones the Previews panel lists, in
// which order, and the route changes behind selecting, expanding and collapsing one. Pure.

import type { Preview } from '@vibeke/core';
import type { WorkspaceRoute } from '../router';
import type { WorkspaceRow } from './workspaces';

/** Live and declared first, then those that stopped answering, then discovery's suggestions. */
const RANK: Record<string, number> = { up: 0, declared: 1, down: 2, suggested: 3 };
const rank = (v: Pick<Preview, 'status'>): number => RANK[v.status] ?? 2;

export function previewLabel(v: Pick<Preview, 'label' | 'url'>): string {
  if (v.label) return v.label;
  try {
    const u = new URL(v.url);
    return `${u.host}${u.pathname === '/' ? '' : u.pathname}`;
  } catch {
    return v.url;
  }
}

/** The panel's order: by status rank, then port; stable for equal keys. */
export function orderPreviews<T extends Pick<Preview, 'status' | 'port'>>(list: readonly T[]): T[] {
  return list
    .map((v, i) => ({ v, i }))
    .sort((a, b) => rank(a.v) - rank(b.v) || a.v.port - b.v.port || a.i - b.i)
    .map((x) => x.v);
}

/** Previews that belong to the workspace's panes, without gone ones, in the panel's order. */
export function workspacePreviews(row: Pick<WorkspaceRow, 'panes'>, previews: readonly Preview[] | undefined): Preview[] {
  const panes = new Set(row.panes.map((p) => p.pane.id));
  return orderPreviews((previews ?? []).filter((v) => v.pane && panes.has(v.pane) && v.status !== 'gone'));
}

/** The number on the header's Preview button: previews that answer now. */
export const livePreviewCount = (list: readonly Pick<Preview, 'status'>[]): number => list.filter((v) => v.status === 'up').length;

export const isSuggested = (v: Pick<Preview, 'status'>): boolean => v.status === 'suggested';

/** One embedded view or phone session per host, machine and preview. */
export const previewKey = (hostId: string, v: Pick<Preview, 'id' | 'machine'>): string => `${hostId}:${v.machine ?? ''}:${v.id}`;

/** `machine` for `preview.*` calls, when the preview names one. */
export const machineParam = (v: Pick<Preview, 'machine'>): { machine?: string } => (v.machine ? { machine: v.machine } : {});

/** The selected preview of a route, or null when none is selected or it is gone. */
export function selectedPreview(route: Pick<WorkspaceRoute, 'preview'>, list: readonly Preview[]): Preview | null {
  return route.preview ? (list.find((v) => v.id === route.preview) ?? null) : null;
}

/** Select a preview (null = back to the list). An expanded preview stays expanded when another is picked. */
export function selectPreviewRoute(route: WorkspaceRoute, id: string | null): WorkspaceRoute {
  return { ...route, preview: id, expanded: !!id && !!route.expanded };
}

/** Expand: the selected preview fills the centre, and the panel closes to give it the width. */
export function expandPreviewRoute(route: WorkspaceRoute): WorkspaceRoute {
  return route.preview ? { ...route, expanded: true, panel: 'off', file: null, commit: null } : route;
}

/** Collapse: the preview goes back into the Previews panel. */
export function collapsePreviewRoute(route: WorkspaceRoute): WorkspaceRoute {
  return { ...route, expanded: false, panel: 'previews', file: null, commit: null };
}

/** True when the centre shows the selected preview instead of the tab. */
export const showsExpandedPreview = (route: Pick<WorkspaceRoute, 'preview' | 'expanded'>, list: readonly Preview[]): boolean =>
  !!route.expanded && !!selectedPreview(route, list);
