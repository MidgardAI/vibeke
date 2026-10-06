// The workspace's tab strip: one tab per agent (harness glyph + name) with a secondary
// "Terminal" tab for its pane, one per shell pane, and one per dev-server preview. `+` starts an
// agent or a terminal (`tab.create`); ⋯ renames, closes or focuses the host tab behind the
// selected one (hidden for hosts without `tab.*` and for devices without full scope).

import { useEffect, useRef, useState, type KeyboardEvent } from 'react';
import { Bot, Crosshair, Globe, MoreHorizontal, Pencil, Plus, SquareTerminal, Trash2 } from 'lucide-react';
import type { Preview } from '@vibeke/core';
import { HarnessIcon, StatusDot, cx, type Status } from '../../components/ui';
import { t } from '../../i18n';
import { TAB_PANEL_ID, rovingTab, tabDomId, tabKeyTarget } from '../../lib/tabs-nav';
import type { PaneRow } from '../../lib/tree';
import type { WorkspaceRow } from '../../lib/workspaces';
import { MenuButton, type MenuItem } from './menu';

export type TabKind = 'agent' | 'term' | 'preview';

export interface WsTab {
  /** `a:<pane>`, `t:<pane>`, `p:<preview>`. */
  id: string;
  kind: TabKind;
  pane: PaneRow | null;
  preview: Preview | null;
  label: string;
  title: string;
  /** Secondary terminal tab of an agent pane (rendered smaller). */
  secondary: boolean;
  status: Status | null;
}

const PREVIEW_HIDDEN = new Set(['gone', 'suggested']);

export const paneLabel = (p: PaneRow): string => p.pane.title ?? p.run?.name ?? (p.run ? p.run.harness : p.pane.auto_title);

export function paneStatus(p: PaneRow): Status | null {
  if (p.attention === 'interaction' || p.run?.execution.value === 'error') return 'need';
  if (p.attention === 'working') return 'working';
  return null;
}

/** Previews that belong to the workspace's panes and are worth a tab. */
export function workspacePreviews(row: WorkspaceRow, previews: readonly Preview[] | undefined): Preview[] {
  const panes = new Set(row.panes.map((p) => p.pane.id));
  return (previews ?? []).filter((v) => v.pane && panes.has(v.pane) && !PREVIEW_HIDDEN.has(v.status));
}

export function previewLabel(v: Preview): string {
  if (v.label) return v.label;
  try {
    const u = new URL(v.url);
    return `${u.host}${u.pathname === '/' ? '' : u.pathname}`;
  } catch {
    return v.url;
  }
}

/** Tabs in display order: agents (with their terminal), shells, previews. */
export function workspaceTabs(row: WorkspaceRow, previews: readonly Preview[]): WsTab[] {
  const out: WsTab[] = [];
  const agents = row.panes.filter((p) => p.run);
  const shells = row.panes.filter((p) => !p.run);
  for (const p of agents) {
    const label = paneLabel(p);
    out.push({ id: `a:${p.pane.id}`, kind: 'agent', pane: p, preview: null, label, title: label, secondary: false, status: paneStatus(p) });
    out.push({ id: `t:${p.pane.id}`, kind: 'term', pane: p, preview: null, label: t.tabs2.terminal, title: t.tabs2.terminalOf(label), secondary: true, status: null });
  }
  for (const p of shells) {
    const label = p.pane.title ?? (p.pane.fg_cmdline.length ? p.pane.fg_cmdline.join(' ') : p.pane.auto_title);
    out.push({ id: `t:${p.pane.id}`, kind: 'term', pane: p, preview: null, label, title: label, secondary: false, status: paneStatus(p) });
  }
  for (const v of previews) {
    const label = previewLabel(v);
    out.push({ id: `p:${v.id}`, kind: 'preview', pane: row.panes.find((p) => p.pane.id === v.pane) ?? null, preview: v, label, title: v.url, secondary: false, status: null });
  }
  return out;
}

export function TabStrip({
  tabs,
  current,
  onSelect,
  onNewAgent,
  onNewTerminal,
  tabMenu,
  locked,
}: {
  tabs: WsTab[];
  current: string;
  onSelect(tab: WsTab): void;
  onNewAgent?: () => void;
  onNewTerminal?: () => void;
  /** Rename / close / focus of the selected tab's host tab; null hides ⋯. */
  tabMenu: { rename(): void; close(): void; focus(): void } | null;
  locked?: boolean;
}) {
  const add: MenuItem[] = [];
  if (onNewAgent) add.push({ label: t.tabs2.newAgent, icon: <Bot />, onSelect: onNewAgent });
  if (onNewTerminal) add.push({ label: t.tabs2.newTerminal, icon: <SquareTerminal />, onSelect: onNewTerminal });

  // Roving focus (WAI-ARIA tabs, manual activation): arrows move focus, Enter/Space select.
  const listRef = useRef<HTMLDivElement>(null);
  const [focused, setFocused] = useState<string | null>(null);
  const ids = tabs.map((x) => x.id);
  const roving = rovingTab(ids, current, focused);
  const focusTab = (id: string) => {
    setFocused(id);
    const el = listRef.current?.querySelector<HTMLElement>(`[data-tab="${CSS.escape(id)}"]`);
    el?.focus();
    el?.scrollIntoView?.({ block: 'nearest', inline: 'nearest' });
  };
  const onKeyDown = (e: KeyboardEvent) => {
    const from = (e.target as HTMLElement).closest<HTMLElement>('[data-tab]')?.dataset.tab ?? null;
    const to = tabKeyTarget(ids, from, e.key);
    if (!to || e.altKey || e.metaKey || e.ctrlKey) return;
    e.preventDefault();
    e.stopPropagation();
    focusTab(to);
  };
  // Focus recovery: the focused tab went away (closed, its agent exited) and focus fell to the
  // page — put it back on the selected tab (or the first) instead of losing the user's place.
  useEffect(() => {
    if (!focused || ids.includes(focused)) return;
    setFocused(null);
    const active = document.activeElement;
    if (active && active !== document.body && !listRef.current?.contains(active)) return;
    const next = rovingTab(ids, current, null);
    if (next) focusTab(next);
  }, [ids.join('\u0000'), current]);

  return (
    <div className="flex h-9 shrink-0 items-center gap-1 border-b border-border pl-2 pr-1.5">
      <div
        ref={listRef}
        role="tablist"
        aria-label={t.workspace.tabs}
        aria-orientation="horizontal"
        onKeyDown={onKeyDown}
        onBlur={(e) => {
          // Focus moved elsewhere on purpose: forget the strip's focus (removal blurs to nothing).
          if (e.relatedTarget && !listRef.current?.contains(e.relatedTarget as Node)) setFocused(null);
        }}
        className="no-scrollbar flex min-w-0 items-center gap-0.5 overflow-x-auto"
      >
        {tabs.map((tab) => {
          const on = tab.id === current;
          return (
            <button
              key={tab.id}
              type="button"
              role="tab"
              id={tabDomId(tab.id)}
              aria-selected={on}
              aria-controls={on ? TAB_PANEL_ID : undefined}
              tabIndex={tab.id === roving ? 0 : -1}
              title={tab.title}
              data-tab={tab.id}
              onFocus={() => setFocused(tab.id)}
              onClick={() => onSelect(tab)}
              className={cx(
                'vk-focus inline-flex h-7 max-w-[220px] shrink-0 items-center gap-1.5 rounded-md px-2.5 text-[13px] pointer-coarse:h-8',
                on ? 'bg-selected text-fg' : 'text-muted hover:bg-hover hover:text-fg',
                tab.secondary && !on && 'text-faint',
              )}
            >
              {tab.kind === 'agent' ? (
                <HarnessIcon harness={tab.pane?.run?.harness ?? null} />
              ) : tab.kind === 'preview' ? (
                <Globe aria-hidden className="size-3.5 shrink-0" strokeWidth={1.75} />
              ) : (
                <SquareTerminal aria-hidden className="size-3.5 shrink-0" strokeWidth={1.75} />
              )}
              <span className={cx('truncate', tab.kind === 'preview' && 'font-mono text-xs')}>{tab.label}</span>
              {tab.status && <StatusDot status={tab.status} />}
            </button>
          );
        })}
      </div>
      {!locked && add.length > 0 && <MenuButton label={t.tabs2.newTab} icon={<Plus />} align="left" items={add} />}
      <span className="flex-1" />
      {!locked && tabMenu && (
        <MenuButton
          label={t.tabs2.tabMenu}
          icon={<MoreHorizontal />}
          items={[
            { label: t.tabs2.renameTab, icon: <Pencil />, onSelect: tabMenu.rename },
            { label: t.tabs2.focusTab, icon: <Crosshair />, onSelect: tabMenu.focus },
            'sep',
            { label: t.tabs2.closeTab, icon: <Trash2 />, tone: 'danger', onSelect: tabMenu.close },
          ]}
        />
      )}
    </div>
  );
}
