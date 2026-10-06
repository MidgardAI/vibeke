// Three-pane shell: workspace sidebar | centre | right panel (Changes / Files).
// ≥1100px: inline sidebar (280) + centre + docked, resizable panel (width per device).
// 960–1099px: inline sidebar, the panel overlays the centre.
// <960px: the sidebar is a drawer (menu button) and the panel covers the screen.

import { useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from 'react';
import { FileDiff, FolderTree, PanelRightClose, Plus, Server } from 'lucide-react';
import { Dialog } from '../components/dialog';
import { Button, Empty, HarnessIcon, IconButton, Spinner, StatusDot, Tabs, cx } from '../components/ui';
import { t } from '../i18n';
import { PANEL_MAX, PANEL_MIN } from '../lib/prefs';
import { keyLabel } from '../lib/shortcuts';
import type { PaneRow } from '../lib/tree';
import type { WorkspaceRow } from '../lib/workspaces';
import { navigate, type PaneView, type PanelKind, type Route, type WorkspaceRoute } from '../router';
import { ChangesPanel } from '../screens/changes';
import { PaneScreen } from '../screens/pane/pane-screen';
import { useApp, useHost, usePrefs } from './hooks';
import { emitUi, isMacLike } from './keyboard';
import { currentLayoutMode, drawerOpen, effectivePanel, rememberTab, selectedPane, togglePanelRoute, useDrawer, useWorkspaceRows, type LayoutMode } from './selection';
import { MenuButton, WideContext, useMediaQuery, useWide } from './shell';
import { Sidebar } from './sidebar';

export function useLayoutMode(): LayoutMode {
  const wide = useMediaQuery('(min-width: 1100px)');
  const mid = useMediaQuery('(min-width: 960px)');
  return wide ? 'wide' : mid ? 'mid' : 'narrow';
}

/** Toggle the right panel of the current workspace route (⌘3, buttons). */
export function useTogglePanel(): (route: Route, kind?: PanelKind) => void {
  const app = useApp();
  return useCallback(
    (route: Route, kind: PanelKind = 'changes') => {
      if (route.name !== 'workspace') return;
      const mode = currentLayoutMode();
      const next = togglePanelRoute(route, app.prefs.get(), mode, kind);
      if (next.panelOpen !== undefined) app.prefs.patch({ panelOpen: next.panelOpen });
      navigate(next.route, { replace: true });
    },
    [app],
  );
}

export function Layout({ route, children }: { route: Route; children: ReactNode }) {
  const prefs = usePrefs();
  const mode = useLayoutMode();
  const drawer = useDrawer();
  const inline = mode !== 'narrow' && !prefs.sidebarHidden;
  const panel = route.name === 'workspace' ? effectivePanel(route, prefs, mode) : null;

  // Leaving the narrow layout closes the drawer; so does any navigation.
  useEffect(() => {
    if (mode !== 'narrow') drawerOpen.set(false);
  }, [mode]);

  return (
    <WideContext.Provider value={inline}>
      <div className="flex h-full">
        {inline && <Sidebar route={route} mode="inline" />}
        <div className="app-content relative flex h-full min-w-0 flex-1 flex-col">
          {children}
          {panel && route.name === 'workspace' && mode === 'mid' && (
            <div className="app-panel animate-drawer-r absolute inset-y-0 right-0 z-30 flex w-[420px] max-w-full flex-col border-l border-border bg-bg shadow-[var(--shadow)]">
              <RightPanel route={route} kind={panel} />
            </div>
          )}
        </div>
        {panel && route.name === 'workspace' && mode === 'wide' && <DockedPanel route={route} kind={panel} />}
        {panel && route.name === 'workspace' && mode === 'narrow' && (
          <Dialog
            open
            onClose={() => navigate({ ...route, panel: null, file: null, commit: null }, { replace: true })}
            label={t.workspace.panel}
            className="fixed inset-0 z-40 flex"
            panelClassName="app-panel animate-drawer-r flex h-full w-full flex-col bg-bg pt-safe outline-none"
          >
            <RightPanel route={route} kind={panel} />
          </Dialog>
        )}
        <Dialog
          open={mode === 'narrow' && drawer}
          onClose={() => drawerOpen.set(false)}
          label={t.sidebar.label}
          className="vk-scrim fixed inset-0 z-50 flex"
          panelClassName="animate-drawer-l flex h-full w-[300px] max-w-[86vw] flex-col shadow-[var(--shadow)] outline-none"
        >
          <Sidebar route={route} mode="drawer" />
        </Dialog>
      </div>
    </WideContext.Provider>
  );
}

function DockedPanel({ route, kind }: { route: WorkspaceRoute; kind: PanelKind }) {
  const app = useApp();
  const prefs = usePrefs();
  const [width, setWidth] = useState(prefs.panelWidth);
  const [drag, setDrag] = useState(false);
  const start = useRef<{ x: number; w: number } | null>(null);
  useEffect(() => setWidth(prefs.panelWidth), [prefs.panelWidth]);
  const max = () => Math.max(PANEL_MIN, Math.min(PANEL_MAX, window.innerWidth - (prefs.sidebarHidden ? 0 : 280) - 420));
  const clamp = (w: number) => Math.round(Math.min(max(), Math.max(PANEL_MIN, w)));
  const onDown = (e: ReactPointerEvent) => {
    e.preventDefault();
    (e.target as Element).setPointerCapture?.(e.pointerId);
    start.current = { x: e.clientX, w: width };
    setDrag(true);
  };
  const onMove = (e: ReactPointerEvent) => {
    if (!start.current) return;
    setWidth(clamp(start.current.w + (start.current.x - e.clientX)));
  };
  const onUp = () => {
    if (!start.current) return;
    start.current = null;
    setDrag(false);
    app.prefs.patch({ panelWidth: width });
  };
  const w = typeof window === 'undefined' ? width : clamp(width);
  return (
    <aside aria-label={t.workspace.panel} className="app-panel relative flex h-full shrink-0 flex-col border-l border-border bg-bg" style={{ width: w }}>
      <div
        role="separator"
        aria-orientation="vertical"
        aria-label={t.workspace.resizePanel}
        aria-valuenow={w}
        aria-valuemin={PANEL_MIN}
        aria-valuemax={PANEL_MAX}
        tabIndex={0}
        data-dragging={drag || undefined}
        className="vk-resize absolute inset-y-0 -left-1 z-20 w-2"
        onPointerDown={onDown}
        onPointerMove={onMove}
        onPointerUp={onUp}
        onPointerCancel={onUp}
        onDoubleClick={() => app.prefs.patch({ panelWidth: 420 })}
        onKeyDown={(e) => {
          if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
          e.preventDefault();
          app.prefs.patch({ panelWidth: clamp(w + (e.key === 'ArrowLeft' ? 24 : -24)) });
        }}
      />
      <RightPanel route={route} kind={kind} />
    </aside>
  );
}

/** Changes / Files panel of a workspace (Phase 1: the existing changes list for the selected pane). */
function RightPanel({ route, kind }: { route: WorkspaceRoute; kind: PanelKind }) {
  const app = useApp();
  const rows = useWorkspaceRows();
  const toggle = useTogglePanel();
  const mac = isMacLike(app.platform.mac);
  const row = rows.find((r) => r.host === route.host && r.workspace.id === route.workspace);
  const pane = selectedPane(route, row);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="titlebar flex h-11 shrink-0 items-center gap-1 border-b border-border px-2">
        <Tabs
          label={t.workspace.panel}
          value={kind}
          onChange={(k) => k !== kind && toggle(route, k)}
          items={[
            { value: 'files', label: t.workspace.files, icon: <FolderTree /> },
            { value: 'changes', label: t.workspace.changes, icon: <FileDiff /> },
          ]}
        />
        <span className="flex-1" />
        <IconButton label={`${t.workspace.closePanel} (${keyLabel(mac, 'mod+3')})`} onClick={() => toggle(route, kind)}>
          <PanelRightClose />
        </IconButton>
      </div>
      {kind === 'files' ? (
        <Empty icon={<FolderTree />} title={t.workspace.filesUnsupported} hint={t.workspace.filesUnsupportedHint} />
      ) : pane ? (
        <ChangesPanel only={{ host: route.host, pane }} />
      ) : (
        <Empty icon={<FileDiff />} title={t.changes.noPane} />
      )}
    </div>
  );
}

// ---- centre: a workspace ---------------------------------------------------------------------

export function WorkspaceScreen({ route }: { route: WorkspaceRoute }) {
  const rows = useWorkspaceRows();
  const host = useHost(route.host);
  const row = rows.find((r) => r.host === route.host && r.workspace.id === route.workspace);
  const pane = selectedPane(route, row);

  useEffect(() => {
    if (row && pane && row.panes.some((p) => p.pane.id === pane)) rememberTab(route.host, route.workspace, pane);
  }, [row?.key, pane]);

  if (!row) {
    if (!host || !host.dashboard)
      return (
        <Centre>
          <div className="flex flex-1 items-center justify-center">
            <Spinner />
          </div>
        </Centre>
      );
    return (
      <Centre title={t.workspace.notFound}>
        <Empty icon={<Server />} title={t.workspace.notFound} hint={t.workspace.notFoundHint} action={<Button onClick={() => navigate({ name: 'inbox' })}>{t.tabs.inbox}</Button>} />
      </Centre>
    );
  }
  const current = row.panes.find((p) => p.pane.id === pane);
  if (!current) {
    return (
      <Centre title={row.title} sub={row.hostName}>
        <Empty
          title={t.workspace.noPanes}
          action={
            <Button variant="primary" icon={<Plus />} onClick={() => emitUi('new-agent')}>
              {t.panes.newAgent}
            </Button>
          }
        />
      </Centre>
    );
  }
  return <WorkspacePane key={`${row.key}/${current.pane.id}`} route={route} row={row} current={current} />;
}

function WorkspacePane({ route, row, current }: { route: WorkspaceRoute; row: WorkspaceRow; current: PaneRow }) {
  const app = useApp();
  const prefs = usePrefs();
  const wide = useWide();
  const mode = useLayoutMode();
  const toggle = useTogglePanel();
  const mac = isMacLike(app.platform.mac);
  // Phase 1: the agent view is the existing terminal/history pair; the panel holds Changes.
  const [view, setView] = useState<PaneView>('term');
  const panelOpen = !!effectivePanel(route, prefs, mode);
  const repo = row.workspace.root_path.split('/').filter(Boolean).pop() ?? '';
  const sub = [repo !== row.title ? repo : null, row.hostName].filter(Boolean).join(' · ');
  return (
    <PaneScreen
      host={row.host}
      pane={current.pane.id}
      view={view}
      embed={{
        title: row.title,
        sub,
        onView: setView,
        onChanges: () => !panelOpen && toggle(route, 'changes'),
        leading: wide ? undefined : <MenuButton />,
        trailing: (
          <IconButton label={`${t.sidebar.togglePanel} (${keyLabel(mac, 'mod+3')})`} active={panelOpen} onClick={() => toggle(route, 'changes')}>
            <FileDiff />
          </IconButton>
        ),
        tabs: <PaneTabs route={route} row={row} current={current.pane.id} />,
      }}
    />
  );
}

/** One tab per pane of the workspace (agents first by layout order); Phase 2 replaces it. */
function PaneTabs({ route, row, current }: { route: WorkspaceRoute; row: WorkspaceRow; current: string }) {
  return (
    <div role="tablist" aria-label={t.workspace.tabs} className="no-scrollbar flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto">
      {row.panes.map((p) => {
        const on = p.pane.id === current;
        const label = p.pane.title ?? p.run?.name ?? (p.run ? p.run.harness : p.pane.auto_title);
        const status = p.attention === 'interaction' || p.run?.execution.value === 'error' ? 'need' : p.attention === 'working' ? 'working' : null;
        return (
          <button
            key={p.key}
            type="button"
            role="tab"
            aria-selected={on}
            title={label}
            onClick={() => navigate({ ...route, pane: p.pane.id }, { replace: true })}
            className={cx('vk-focus inline-flex h-7 max-w-[200px] shrink-0 items-center gap-1.5 rounded-md px-2.5 text-sm', on ? 'bg-selected text-fg' : 'text-muted hover:bg-hover hover:text-fg')}
          >
            <HarnessIcon harness={p.run?.harness ?? null} />
            <span className="truncate">{label}</span>
            {status && <StatusDot status={status} />}
          </button>
        );
      })}
    </div>
  );
}

function Centre({ title, sub, children }: { title?: string; sub?: string; children: ReactNode }) {
  const wide = useWide();
  return (
    <div className="flex h-full min-h-0 flex-col pt-safe">
      <header className={cx('titlebar flex h-11 shrink-0 items-center gap-2 border-b border-border px-3', !wide && 'titlebar-inset pl-1.5')}>
        {!wide && <MenuButton />}
        <div className="flex min-w-0 flex-1 items-baseline gap-2">
          {title && <h1 className="truncate text-base font-semibold">{title}</h1>}
          {sub && <span className="truncate text-sm text-muted">{sub}</span>}
        </div>
      </header>
      {children}
    </div>
  );
}
