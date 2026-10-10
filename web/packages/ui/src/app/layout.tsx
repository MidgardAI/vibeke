// Three-pane shell: workspace sidebar | centre | right panel (Changes / Files).
// ≥1100px: inline sidebar (280) + centre + docked, resizable panel (width per device).
// 960–1099px: inline sidebar, the panel overlays the centre.
// <960px: the sidebar is a drawer (menu button) and the panel covers the screen.

import { Suspense, lazy, useCallback, useEffect, useRef, useState, type PointerEvent as ReactPointerEvent, type ReactNode } from 'react';
import { Dialog } from '../components/dialog';
import { Spinner } from '../components/ui';
import { t } from '../i18n';
import { PANEL_MAX, PANEL_MIN } from '../lib/prefs';
import { navigate, type PanelKind, type Route, type WorkspaceRoute } from '../router';
import { useApp, usePrefs } from './hooks';
import { currentLayoutMode, drawerOpen, effectivePanel, togglePanelRoute, useDrawer, type LayoutMode } from './selection';
import { WideContext, useMediaQuery } from './shell';
import { Sidebar } from './sidebar';

const LazyRightPanel = lazy(() => import('../screens/workspace/panel/right-panel'));

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
            <RightPanel route={route} kind={panel} sheet />
          </Dialog>
        )}
        <Dialog
          open={mode === 'narrow' && drawer}
          onClose={() => drawerOpen.set(false)}
          label={t.sidebar.label}
          closeOnNavigate
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

/** Changes / Files panel of a workspace (lazy: screens/workspace/panel). */
function RightPanel({ route, kind, sheet }: { route: WorkspaceRoute; kind: PanelKind; sheet?: boolean }) {
  return (
    <Suspense
      fallback={
        <div className="flex flex-1 items-center justify-center">
          <Spinner />
        </div>
      }
    >
      <LazyRightPanel route={route} kind={kind} sheet={sheet} />
    </Suspense>
  );
}

// ---- centre: a workspace ---------------------------------------------------------------------

export { WorkspaceScreen } from '../screens/workspace/workspace-screen';
