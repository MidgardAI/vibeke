// Root component: shells mount <VibekeApp platform={…}/> and nothing else (spec 16 §9.3).

import { useEffect, useMemo, useRef, type ReactNode } from 'react';
import { AlertTriangle } from 'lucide-react';
import { Empty, Spinner } from '../components/ui';
import { t } from '../i18n';
import { useStore } from '../lib/store';
import type { UiPlatform } from '../platform';
import { hashFromUrl, navigate, useRoute, type Route, type Tab } from '../router';
import { ChangesScreen } from '../screens/changes';
import { InboxScreen } from '../screens/inbox';
import { CrewScreen, IdleLockOverlay, InteractionRoute, RunRoute, Tour, useIdleLock } from '../screens/misc';
import { PairScreen } from '../screens/pair';
import { FocusScreen, PanesScreen } from '../screens/panes';
import { PaneScreen } from '../screens/pane/pane-screen';
import { SettingsScreen } from '../screens/settings';
import { AppContext, useApp, useHosts, useInboxItems, usePrefs } from './hooks';
import { AppModel } from './model';
import { BusyBar, ConnectionBanner, TabBar, Toasts, TopBar, useThemeEffect } from './shell';

export function VibekeApp({ platform, model }: { platform: UiPlatform; model?: AppModel }) {
  const app = useMemo(() => model ?? new AppModel(platform), [platform, model]);
  useEffect(() => {
    void app.start();
    return () => app.stop();
  }, [app]);
  return (
    <AppContext.Provider value={app}>
      <Boot />
    </AppContext.Provider>
  );
}

function Boot() {
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
  return <Main />;
}

const TAB_TITLES: Record<Tab, string> = { inbox: t.tabs.inbox, panes: t.tabs.panes, focus: t.tabs.focus, changes: t.tabs.changes };

function Main() {
  const app = useApp();
  const prefs = usePrefs();
  const route = useRoute();
  const hosts = useHosts();
  const items = useInboxItems();
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
    <div className="flex h-full flex-col">
      <Screen route={route} />
      <Toasts />
      <IdleLockOverlay />
      {hosts.length > 0 && ['inbox', 'panes', 'focus', 'changes'].includes(route.name) && <Tour />}
    </div>
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
        <Framed title={t.settings.title} route={route}>
          <SettingsScreen />
        </Framed>
      );
    case 'crew':
      return (
        <Framed title={t.crew.title} route={route}>
          <CrewScreen />
        </Framed>
      );
    case 'inbox':
    case 'panes':
    case 'focus':
    case 'changes':
      return (
        <Framed title={TAB_TITLES[route.name]} route={route} tab={route.name} scroll={route.name !== 'changes'}>
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

function Framed({ title, route, tab, children, scroll = true }: { title: string; route: Route; tab?: Tab; children: ReactNode; scroll?: boolean }) {
  return (
    <div className="flex h-full flex-col pt-safe px-safe">
      <TopBar title={title} route={route} />
      <ConnectionBanner />
      <BusyBar />
      <main className={scroll ? 'min-h-0 flex-1 overflow-y-auto' : 'flex min-h-0 flex-1 flex-col'}>{children}</main>
      <TabBar active={tab} />
    </div>
  );
}
