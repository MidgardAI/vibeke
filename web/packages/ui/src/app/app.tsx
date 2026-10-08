// Root component: shells mount <VibekeApp platform={…}/> and nothing else (spec 16 §9.3).

import { useEffect, useMemo, useRef, type ReactNode } from 'react';
import { AlertTriangle } from 'lucide-react';
import { Empty, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { useStore } from '../lib/store';
import type { UiPlatform } from '../platform';
import { mostUrgent, workspaceOfPane } from '../lib/workspaces';
import { formatRoute, hashFromUrl, navigate, useRoute, workspaceRoute, type Route } from '../router';
import { InboxScreen } from '../screens/inbox';
import { IncomingScreen } from '../screens/incoming';
import { CrewScreen, IdleLockOverlay, InteractionRoute, RunRoute, Tour, useIdleLock } from '../screens/misc';
import { PairScreen } from '../screens/pair';
import { SettingsScreen } from '../screens/settings';
import { QuickScreen } from '../screens/quick';
import { ApprovalScreen } from '../screens/approve';
import { useApprovalCount, useApprovalStores } from './approval-stores';
import { useHandoffStores } from './handoff-stores';
import { AppContext, useApp, useHosts, useInboxItems, usePrefs } from './hooks';
import { KeyboardLayer, type Surface } from './keyboard';
import { AppModel } from './model';
import { Layout, WorkspaceScreen } from './layout';
import { lastWorkspace, rememberTab, resolveLegacy, useWorkspaceRows } from './selection';
import { BusyBar, ConnectionBanner, Toasts, TopBar, useThemeEffect } from './shell';
import { SurfaceContext } from './surface';

/**
 * `surface`: `full` (the app), `quick` (menu-bar quick approvals: inbox only, Esc closes) or
 * `pane` (a popped-out pane window: the pane only; other routes go to the main window).
 */
export function VibekeApp({ platform, model, surface = 'full' }: { platform: UiPlatform; model?: AppModel; surface?: Surface }) {
  const app = useMemo(() => model ?? new AppModel(platform), [platform, model]);
  useEffect(() => {
    void app.start();
    return () => app.stop();
  }, [app]);
  return (
    <AppContext.Provider value={app}>
      <SurfaceContext.Provider value={surface}>
        <Boot surface={surface} />
      </SurfaceContext.Provider>
    </AppContext.Provider>
  );
}

function Boot({ surface }: { surface: Surface }) {
  const app = useApp();
  const phase = useStore(app.phase);
  if (phase === 'loading')
    return (
      <div className="flex h-full flex-col items-center justify-center gap-3">
        <div className="text-2xl font-semibold tracking-tight">{t.appName}</div>
        <Spinner />
      </div>
    );
  if (phase === 'error') return <Empty icon={<AlertTriangle className="size-10" />} title={t.unknownError} hint={app.error ?? undefined} />;
  if (surface === 'quick') return <Quick />;
  if (surface === 'pane') return <PaneWindow />;
  return <Main />;
}

function Quick() {
  const prefs = usePrefs();
  useThemeEffect(prefs.theme, prefs.termFont);
  return (
    <>
      <QuickScreen />
      <KeyboardLayer surface="quick" />
    </>
  );
}

/**
 * A popped-out pane window is bound to the one pane it was opened for (the main process keys the
 * window, its bounds and "pop out again" by that pane): the workspace screen locked to that
 * pane's tabs (conversation, terminal). Any other route, including a different pane, opens in
 * the main window.
 */
function PaneWindow() {
  const app = useApp();
  const prefs = usePrefs();
  const route = useRoute();
  const rows = useWorkspaceRows();
  const bound = useRef<{ host: string; pane: string } | null>(null);
  const home = useRef<string | null>(null);
  useThemeEffect(prefs.theme, prefs.termFont);
  if (!bound.current && route.name === 'pane') bound.current = { host: route.host, pane: route.pane };
  const own = route.name === 'pane' && route.host === bound.current?.host && route.pane === bound.current?.pane;
  useEffect(() => {
    if (own) {
      home.current = formatRoute(route);
      return;
    }
    if (route.name === 'home') return;
    app.platform.windows?.openMain?.(formatRoute(route));
    if (home.current) navigate(home.current, { replace: true });
  }, [route, app, own]);
  return (
    <div className="flex h-full flex-col">
      {own && route.name === 'pane' ? <PaneWindowBody host={route.host} pane={route.pane} show={route.show ?? null} rows={rows} /> : <Spinner />}
      <Toasts />
      <KeyboardLayer surface="pane" />
    </div>
  );
}

function PaneWindowBody({ host, pane, show, rows }: { host: string; pane: string; show: string | null; rows: ReturnType<typeof useWorkspaceRows> }) {
  const ws = workspaceOfPane(rows, host, pane);
  const hostState = useHosts().find((h) => h.record.host_id === host);
  if (!ws) {
    if (hostState?.dashboard) return <Empty title={t.pane.notFound} />;
    return (
      <div className="flex flex-1 items-center justify-center">
        <Spinner />
      </div>
    );
  }
  return <WorkspaceScreen route={workspaceRoute(host, ws.workspace.id, { pane, show })} locked />;
}

function Main() {
  const app = useApp();
  const prefs = usePrefs();
  const route = useRoute();
  const hosts = useHosts();
  const items = useInboxItems();
  const rows = useWorkspaceRows();
  useThemeEffect(prefs.theme, prefs.termFont);
  useIdleLock();
  // Incoming handoffs (nav badge) and outgoing jobs (toasts when a sheet closed early).
  useHandoffStores();
  // Approval requests from panes (inbox cards, the review screen).
  useApprovalStores();
  const approvals = useApprovalCount();

  // Notification taps routed into the running app.
  useEffect(() => app.platform.notifications?.onOpen((url) => navigate(hashFromUrl(url))), [app]);

  // Hosts known well enough to pick a destination: a dashboard, or every host settled offline.
  const ready = hosts.some((h) => h.dashboard) || (hosts.length > 0 && hosts.every((h) => h.status !== 'connecting' && h.status !== 'idle'));

  // `#/` → Inbox when anything is open, else the most urgent workspace; no hosts → pairing.
  const decided = useRef(false);
  useEffect(() => {
    if (route.name !== 'home') return;
    if (hosts.length === 0) return navigate({ name: 'pair', d: null }, { replace: true });
    if (!ready && !decided.current) return;
    decided.current = true;
    const top = mostUrgent(rows);
    navigate(items.length || approvals || !top ? { name: 'inbox' } : workspaceRoute(top.host, top.workspace.id), { replace: true });
  }, [route.name, hosts.length, ready, items.length, approvals, rows]);

  // Old routes (Panes, Focus, Changes tabs; pane links) → their workspace.
  const routeKey = formatRoute(route);
  useEffect(() => {
    if (!ready) return;
    const to = resolveLegacy(route, rows, lastWorkspace());
    if (to) navigate(to, { replace: true });
  }, [routeKey, ready, rows]);

  // Remember the workspace this window looked at last (⌘⇧E, the old Changes tab).
  useEffect(() => {
    if (route.name === 'workspace' && route.pane) rememberTab(route.host, route.workspace, route.pane);
  }, [routeKey]);

  return (
    <>
      <Layout route={route}>
        <ConnectionBanner />
        <BusyBar />
        <div className="flex min-h-0 flex-1 flex-col">
          <Screen route={route} />
        </div>
      </Layout>
      <Toasts />
      <IdleLockOverlay />
      {hosts.length > 0 && (route.name === 'inbox' || route.name === 'workspace') && <Tour />}
      <KeyboardLayer surface="full" />
    </>
  );
}

function Screen({ route }: { route: Route }) {
  switch (route.name) {
    case 'workspace':
      return <WorkspaceScreen route={route} />;
    case 'pane':
      // Resolved to its workspace once the dashboard is known (resolveLegacy); unknown panes end here.
      return <PaneRouteFallback host={route.host} />;
    case 'interaction':
      return <InteractionRoute host={route.host} id={route.id} preselect={route.preselect} />;
    case 'run':
      return <RunRoute host={route.host} run={route.run} />;
    case 'pair':
      return (
        <Framed title={t.pair.title}>
          <PairScreen d={route.d} />
        </Framed>
      );
    case 'settings':
      return (
        <Framed title={t.settings.title} width="narrow">
          <SettingsScreen />
        </Framed>
      );
    case 'handoffs':
      return (
        <Framed title={t.incoming.screenTitle} width="narrow">
          <IncomingScreen host={route.host} id={route.id} />
        </Framed>
      );
    case 'approve':
      return (
        <Framed title={t.approve.screenTitle} width="narrow">
          <ApprovalScreen host={route.host} id={route.id} />
        </Framed>
      );
    case 'crew':
      return (
        <Framed title={t.crew.title} width="narrow">
          <CrewScreen />
        </Framed>
      );
    case 'inbox':
      return (
        <Framed title={t.tabs.inbox} sub={<InboxSub />} width="narrow">
          <InboxScreen />
        </Framed>
      );
    case 'panes':
    case 'focus':
    case 'changes':
    case 'home':
      return (
        <div className="flex h-full items-center justify-center">
          <Spinner />
        </div>
      );
    case 'not_found':
      return (
        <Framed title={t.appName}>
          <Empty title="404" hint={route.path} />
        </Framed>
      );
  }
}

function PaneRouteFallback({ host }: { host: string }) {
  const h = useHosts().find((x) => x.record.host_id === host);
  if (!h?.dashboard)
    return (
      <div className="flex h-full items-center justify-center">
        <Spinner />
      </div>
    );
  return (
    <Framed title={t.pane.notFound}>
      <Empty title={t.pane.notFound} />
    </Framed>
  );
}

function InboxSub() {
  const items = useInboxItems();
  const approvals = useApprovalCount();
  return <>{t.quick.needYou(items.length + approvals)}</>;
}

/**
 * Comfortable reading widths on big windows (like native Mac apps): cards and settings stay a
 * column, lists a little wider. The header spans the centre.
 */
const WIDTHS = { narrow: 'max-w-[720px]', medium: 'max-w-[880px]', wide: 'max-w-[1280px]' } as const;
export type FrameWidth = keyof typeof WIDTHS;

function Framed({ title, sub, children, width = 'medium' }: { title: string; sub?: ReactNode; children: ReactNode; width?: FrameWidth }) {
  return (
    <div className="flex h-full min-h-0 flex-col pt-safe px-safe">
      <TopBar title={title} sub={sub} />
      <main className="vk-scroll min-h-0 flex-1 overflow-y-auto">
        <div className={cx('mx-auto w-full pt-2', WIDTHS[width])}>{children}</div>
      </main>
    </div>
  );
}
