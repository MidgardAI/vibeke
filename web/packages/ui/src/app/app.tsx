// Root component: shells mount <VibekeApp platform={…}/> and nothing else (spec 16 §9.3).

import { useEffect, useMemo, useRef, type ReactNode } from 'react';
import { AlertTriangle } from 'lucide-react';
import { Empty, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { useStore } from '../lib/store';
import type { UiPlatform } from '../platform';
import { formatRoute, hashFromUrl, navigate, useRoute, type Route, type Tab } from '../router';
import { ChangesScreen } from '../screens/changes';
import { InboxScreen } from '../screens/inbox';
import { CrewScreen, IdleLockOverlay, InteractionRoute, RunRoute, Tour, useIdleLock } from '../screens/misc';
import { PairScreen } from '../screens/pair';
import { FocusScreen, PanesScreen } from '../screens/panes';
import { PaneScreen } from '../screens/pane/pane-screen';
import { SettingsScreen } from '../screens/settings';
import { QuickScreen } from '../screens/quick';
import { AppContext, useApp, useHosts, useInboxItems, usePrefs } from './hooks';
import { KeyboardLayer, type Surface } from './keyboard';
import { AppModel } from './model';
import { BusyBar, ConnectionBanner, Sidebar, TabBar, Toasts, TopBar, WideContext, useMediaQuery, useThemeEffect, useWide } from './shell';
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
 * window, its bounds and "pop out again" by that pane). Its own views (terminal, history,
 * changes) stay here; any other route, including a different pane, opens in the main window.
 */
function PaneWindow() {
  const app = useApp();
  const prefs = usePrefs();
  const route = useRoute();
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
      {own && route.name === 'pane' ? <PaneScreen host={route.host} pane={route.pane} view={route.view} /> : <Spinner />}
      <Toasts />
      <KeyboardLayer surface="pane" />
    </div>
  );
}

const TAB_TITLES: Record<Tab, string> = { inbox: t.tabs.inbox, panes: t.tabs.panes, focus: t.tabs.focus, changes: t.tabs.changes };

function Main() {
  const app = useApp();
  const prefs = usePrefs();
  const route = useRoute();
  const hosts = useHosts();
  const items = useInboxItems();
  const wide = useMediaQuery('(min-width: 960px)');
  useThemeEffect(prefs.theme, prefs.termFont);
  useIdleLock();

  // Notification taps routed into the running app.
  useEffect(() => app.platform.notifications?.onOpen((url) => navigate(hashFromUrl(url))), [app]);

  // `#/` → Inbox when anything is open, else Panes; no hosts → pairing.
  const decided = useRef(false);
  useEffect(() => {
    if (route.name !== 'home') return;
    if (hosts.length === 0) return navigate({ name: 'pair', d: null }, { replace: true });
    const ready = hosts.some((h) => h.dashboard) || hosts.every((h) => h.status !== 'connecting' && h.status !== 'idle');
    if (!ready && !decided.current) return;
    decided.current = true;
    navigate({ name: items.length ? 'inbox' : 'panes' }, { replace: true });
  }, [route.name, hosts, items.length]);

  return (
    <WideContext.Provider value={wide}>
      <div className="flex h-full">
        {wide && <Sidebar route={route} />}
        <div className="app-content flex h-full min-w-0 flex-1 flex-col">
          <Screen route={route} />
        </div>
        <Toasts />
        <IdleLockOverlay />
        {hosts.length > 0 && ['inbox', 'panes', 'focus', 'changes'].includes(route.name) && <Tour />}
        <KeyboardLayer surface="full" />
      </div>
    </WideContext.Provider>
  );
}

function Screen({ route }: { route: Route }) {
  switch (route.name) {
    case 'pane':
      return <PaneScreen host={route.host} pane={route.pane} view={route.view} />;
    case 'interaction':
      return <InteractionRoute host={route.host} id={route.id} preselect={route.preselect} />;
    case 'run':
      return <RunRoute host={route.host} run={route.run} />;
    case 'pair':
      return (
        <Framed title={t.pair.title} route={route}>
          <PairScreen d={route.d} />
        </Framed>
      );
    case 'settings':
      return (
        <Framed title={t.settings.title} route={route} width="narrow">
          <SettingsScreen />
        </Framed>
      );
    case 'crew':
      return (
        <Framed title={t.crew.title} route={route} width="narrow">
          <CrewScreen />
        </Framed>
      );
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
      return (
        <Framed title={TAB_TITLES[route.name]} route={route} tab={route.name} scroll={route.name !== 'changes'} width={route.name === 'inbox' ? 'narrow' : route.name === 'changes' ? 'wide' : 'medium'}>
          {route.name === 'inbox' && <InboxScreen />}
          {route.name === 'panes' && <PanesScreen />}
          {route.name === 'focus' && <FocusScreen />}
          {route.name === 'changes' && <ChangesScreen />}
        </Framed>
      );
    case 'home':
      return (
        <div className="flex h-full items-center justify-center">
          <Spinner />
        </div>
      );
    case 'not_found':
      return (
        <Framed title={t.appName} route={route}>
          <Empty title="404" hint={route.path} />
        </Framed>
      );
  }
}

/**
 * Comfortable reading widths on big windows (like native Mac apps): cards and settings stay a
 * column, lists a little wider, diffs wider still. The title aligns with the column.
 */
const WIDTHS = { narrow: 'max-w-[760px]', medium: 'max-w-[880px]', wide: 'max-w-[1280px]' } as const;
export type FrameWidth = keyof typeof WIDTHS;

function Framed({ title, route, tab, children, scroll = true, width = 'medium' }: { title: string; route: Route; tab?: Tab; children: ReactNode; scroll?: boolean; width?: FrameWidth }) {
  const wide = useWide();
  const column = cx('mx-auto w-full', WIDTHS[width]);
  return (
    <div className="flex h-full flex-col pt-safe px-safe">
      <TopBar title={title} route={route} column={column} />
      <ConnectionBanner />
      <BusyBar />
      <main className={scroll ? 'min-h-0 flex-1 overflow-y-auto' : 'flex min-h-0 flex-1 flex-col'}>
        <div className={cx(column, !scroll && 'flex min-h-0 flex-1 flex-col')}>{children}</div>
      </main>
      {!wide && <TabBar active={tab} />}
    </div>
  );
}
