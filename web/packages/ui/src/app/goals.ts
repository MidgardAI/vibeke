// Goals per host: `goal.list` for the hosts that support goals (feature `goals`), refreshed while
// the app is visible. A host without a planner answers an error; that host just has no goals.

import { useCallback, useEffect, useMemo, useState } from 'react';
import type { GoalView, HostState } from '@vibeke/core';
import { useStore } from '../lib/store';
import { useAllHosts, useApp } from './hooks';

export interface HostGoals {
  hostId: string;
  hostName: string;
  goals: GoalView[];
}

/** Goals are host-wide: devices with a pane or workspace limit and share devices do not get them. */
export const goalsHost = (h: HostState): boolean =>
  h.status === 'online' && !!h.info?.features.includes('goals') && !h.info.limit?.pane && !h.info.limit?.workspace && h.info.kind !== 'share';

/** Every goals-capable host's goals. `pollMs` = refresh interval while visible. */
export function useGoalLists(pollMs: number): { lists: HostGoals[]; loading: boolean; reload(): void } {
  const app = useApp();
  const hosts = useAllHosts();
  const visible = useStore(app.visible);
  const [data, setData] = useState<Map<string, GoalView[]>>(new Map());
  const [loading, setLoading] = useState(true);
  const [tick, setTick] = useState(0);
  const key = hosts.filter(goalsHost).map((h) => h.record.host_id).join('\u0000');
  const reload = useCallback(() => setTick((n) => n + 1), []);

  useEffect(() => {
    if (!visible) return;
    let live = true;
    const ids = key ? key.split('\u0000') : [];
    const run = async () => {
      const next = new Map<string, GoalView[]>();
      await Promise.all(
        ids.map(async (id) => {
          try {
            const r = await app.conn(id)?.request('goal.list', {});
            if (r) next.set(id, r.goals);
          } catch {
            // no planner on this host, or the call failed: no goals to show
          }
        }),
      );
      if (!live) return;
      setData(next);
      setLoading(false);
    };
    void run();
    const timer = pollMs > 0 ? setInterval(() => void run(), pollMs) : null;
    return () => {
      live = false;
      if (timer) clearInterval(timer);
    };
  }, [app, key, visible, pollMs, tick]);

  const lists = useMemo(
    () =>
      hosts
        .filter(goalsHost)
        .map((h) => ({ hostId: h.record.host_id, hostName: h.info?.host_name ?? h.record.name, goals: data.get(h.record.host_id) ?? [] })),
    [hosts, data],
  );
  return { lists, loading, reload };
}
