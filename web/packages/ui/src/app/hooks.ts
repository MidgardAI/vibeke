import { createContext, useCallback, useContext, useEffect, useMemo, useState } from 'react';
import { groupBatches, rankInbox, type Batch, type HostConnectionApi, type HostState, type InboxItem } from '@vibeke/core';
import { useStore } from '../lib/store';
import { buildTree, runForPane, type PaneTree } from '../lib/tree';
import { overlayHosts, type HostView } from '../lib/offline-cache';
import type { Prefs } from '../lib/prefs';
import type { AppModel } from './model';

export const AppContext = createContext<AppModel | null>(null);

export function useApp(): AppModel {
  const app = useContext(AppContext);
  if (!app) throw new Error('useApp outside <VibekeApp>');
  return app;
}

/**
 * Connections that are not host records (a review connection, spec 20 §7.2): components inside
 * this provider (the phone preview, the notes hooks) talk to it instead of `app.conn`.
 */
export const ConnOverrideContext = createContext<((hostId: string) => HostConnectionApi | undefined) | null>(null);

/** Look up a host's connection, honouring `ConnOverrideContext`. */
export function useConnOf(): (hostId: string) => HostConnectionApi | undefined {
  const app = useApp();
  const override = useContext(ConnOverrideContext);
  return useCallback((hostId: string) => override?.(hostId) ?? app.conn(hostId), [app, override]);
}

/** Every paired host. */
export function useAllHosts(): readonly HostState[] {
  const app = useApp();
  return useStore(app.manager);
}

/**
 * Hosts with dashboards (panes, inbox, banners): every paired host. A host that has not answered
 * yet shows its saved dashboard (`cachedAt` is set), so a cold start offline still lists rows.
 */
export function useHosts(): readonly HostView[] {
  const app = useApp();
  const all = useAllHosts();
  const cached = useStore(app.cached);
  return useMemo(() => overlayHosts(all, cached), [all, cached]);
}

export function useHost(hostId: string): HostView | undefined {
  return useHosts().find((h) => h.record.host_id === hostId);
}

export function usePrefs(): Prefs {
  return useStore(useApp().prefs);
}

/**
 * Re-render every `ms` (wait-time labels, banners). Display-only: paused while the window is
 * hidden (a closed desktop window or a background tab does no work), caught up when shown.
 */
export function useNow(ms = 1000): number {
  const app = useApp();
  const visible = useStore(app.visible);
  const [now, setNow] = useState(() => app.platform.clock.now());
  useEffect(() => {
    if (!visible) return;
    setNow(app.platform.clock.now());
    const id = setInterval(() => setNow(app.platform.clock.now()), ms);
    return () => clearInterval(id);
  }, [app, ms, visible]);
  return now;
}

export function useInboxItems(): InboxItem[] {
  const hosts = useHosts();
  return useMemo(() => inboxItems(hosts), [hosts]);
}

export function inboxItems(hosts: readonly HostState[]): InboxItem[] {
  const items: InboxItem[] = [];
  for (const h of hosts) {
    const d = h.dashboard;
    if (!d) continue;
    for (const i of d.interactions) {
      if (i.status !== 'open') continue;
      items.push({
        host_id: h.record.host_id,
        interaction: i,
        run: d.runs.find((r) => r.id === i.run) ?? runForPane(d, i.pane) ?? undefined,
        pane: d.panes.find((p) => p.id === i.pane),
      });
    }
  }
  return rankInbox(items);
}

export function useBatches(items: InboxItem[]): Batch[] {
  return useMemo(() => groupBatches(items), [items]);
}

export function useTree(): PaneTree {
  const hosts = useHosts();
  const prefs = usePrefs();
  return useMemo(
    () => buildTree(hosts, { pins: new Set(prefs.pins), seenDone: prefs.seenDone }),
    [hosts, prefs.pins, prefs.seenDone],
  );
}

/** Answer-store version (re-render on local answer changes). */
export function useAnswers(): number {
  return useStore(useApp().answers);
}

export function useVisible(): boolean {
  return useStore(useApp().visible);
}
