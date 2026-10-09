// How an agent pane is shown: the structured conversation or the harness's own terminal UI.
// A per-device default (Settings → Appearance) and per-workspace overrides (`<host>/<workspace>`),
// both in prefs. The agent tab renders the chosen view; the strip's toggle switches it.
// `?show=term` / `?show=conversation` pick a body explicitly (deep links); null = the chosen view.

import type { Prefs } from './prefs';

export type AgentView = 'conversation' | 'terminal';

export const AGENT_VIEWS: readonly AgentView[] = ['conversation', 'terminal'];
/** Overrides kept per device (oldest dropped beyond this). */
export const MAX_VIEW_OVERRIDES = 200;

export const isAgentView = (v: unknown): v is AgentView => v === 'conversation' || v === 'terminal';
export const otherView = (v: AgentView): AgentView => (v === 'conversation' ? 'terminal' : 'conversation');
export const viewKey = (host: string, workspace: string): string => `${host}/${workspace}`;

/** The workspace's view: its override, else the device default. */
export function agentViewFor(prefs: Pick<Prefs, 'agentView' | 'agentViews'>, host: string, workspace: string): AgentView {
  return prefs.agentViews[viewKey(host, workspace)] ?? prefs.agentView;
}

export function hasViewOverride(prefs: Pick<Prefs, 'agentViews'>, host: string, workspace: string): boolean {
  return viewKey(host, workspace) in prefs.agentViews;
}

/** The overrides with `host/workspace` set to `view` (null clears it, back to the default). */
export function withViewOverride(map: Readonly<Record<string, AgentView>>, host: string, workspace: string, view: AgentView | null): Record<string, AgentView> {
  const k = viewKey(host, workspace);
  const next: Record<string, AgentView> = {};
  for (const [key, v] of Object.entries(map)) if (key !== k) next[key] = v;
  if (view) next[k] = view;
  const keys = Object.keys(next);
  for (const old of keys.slice(0, Math.max(0, keys.length - MAX_VIEW_OVERRIDES))) delete next[old];
  return next;
}

/** What a `?show=` value asks the pane body to be (null = no explicit body). */
export function showBody(show: string | null | undefined): AgentView | null {
  if (show === 'term') return 'terminal';
  if (show === 'conversation') return 'conversation';
  return null;
}

/** The `?show=` value for a body, given the workspace's view (the chosen view needs none). */
export function showFor(body: AgentView, view: AgentView): string | null {
  if (body === view) return null;
  return body === 'terminal' ? 'term' : 'conversation';
}

/**
 * The tab id the route selects for an agent or shell pane: `a:<pane>` (primary, the chosen view),
 * `t:<pane>` (terminal: a shell, or an agent's secondary when the view is conversation),
 * `c:<pane>` (an agent's secondary conversation when the view is terminal), `p:<preview>`.
 */
export function paneTabId(pane: string, isAgent: boolean, show: string | null | undefined, view: AgentView): string {
  if (show?.startsWith('preview:')) return `p:${show.slice('preview:'.length)}`;
  if (!isAgent) return `t:${pane}`;
  const body = showBody(show);
  if (!body || body === view) return `a:${pane}`;
  return body === 'terminal' ? `t:${pane}` : `c:${pane}`;
}

/** What a tab id shows in the centre. */
export function tabBody(id: string, view: AgentView): AgentView | 'preview' {
  if (id.startsWith('p:')) return 'preview';
  if (id.startsWith('a:')) return view;
  if (id.startsWith('c:')) return 'conversation';
  return 'terminal';
}

/** Palette commands for a workspace with agents: both views (the flip marked), and "use default" when overridden. */
export function agentViewCommands(
  prefs: Pick<Prefs, 'agentView' | 'agentViews'>,
  host: string,
  workspace: string,
): { id: 'view-conversation' | 'view-terminal' | 'view-default'; request: AgentView | 'default'; flip: boolean }[] {
  const view = agentViewFor(prefs, host, workspace);
  const out: { id: 'view-conversation' | 'view-terminal' | 'view-default'; request: AgentView | 'default'; flip: boolean }[] = [
    { id: 'view-conversation', request: 'conversation', flip: view === 'terminal' },
    { id: 'view-terminal', request: 'terminal', flip: view === 'conversation' },
  ];
  if (hasViewOverride(prefs, host, workspace)) out.push({ id: 'view-default', request: 'default', flip: false });
  return out;
}
