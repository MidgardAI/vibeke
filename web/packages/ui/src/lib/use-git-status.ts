// `git.status` for one pane, shared by every consumer of the same host/pane (the right panel,
// turn footers…): one request in flight at a time, the last answer cached. Polling runs every
// 5 s only while a polling consumer is mounted, the window is visible and `active` is true; a
// completed agent turn on the pane (its run's `turns_completed` moving) refreshes at once.

import { useCallback, useEffect, useState } from 'react';
import type { GitStatus } from '@vibeke/core';
import { useApp, useHost, useVisible } from '../app/hooks';
import { errorMessage } from './answer';

export const GIT_STATUS_REFRESH_MS = 5000;

export interface GitStatusState {
  status: GitStatus | null;
  error: string | null;
  /** No answer yet (first load). */
  loading: boolean;
  /** Fetch now (refresh button). */
  refresh(): void;
}

interface Entry {
  status: GitStatus | null;
  error: string | null;
  loaded: boolean;
  inflight: Promise<void> | null;
  listeners: Set<() => void>;
}

const cache = new Map<string, Entry>();

function entry(key: string): Entry {
  let e = cache.get(key);
  if (!e) {
    e = { status: null, error: null, loaded: false, inflight: null, listeners: new Set() };
    cache.set(key, e);
  }
  return e;
}

/**
 * Status of `pane` on `host`. `poll: false` reads the shared answer and fetches once on mount
 * (for consumers that ride along with the panel's polling).
 */
export function useGitStatus(host: string | null, pane: string | null, o: { active?: boolean; poll?: boolean } = {}): GitStatusState {
  const app = useApp();
  const visible = useVisible();
  const hostState = useHost(host ?? '');
  const online = hostState?.status === 'online';
  const turns = hostState?.dashboard?.runs.find((r) => r.pane === pane)?.turns_completed ?? 0;
  const key = host && pane ? `${host}\u0000${pane}` : null;
  const [, bump] = useState(0);
  const active = o.active !== false;
  const poll = o.poll !== false;

  const fetch = useCallback(() => {
    if (!key || !host || !pane) return;
    const e = entry(key);
    if (e.inflight) return;
    const conn = app.conn(host);
    if (!conn) return;
    const notify = () => {
      for (const l of [...e.listeners]) l();
    };
    e.inflight = conn
      .request('git.status', { pane })
      .then(
        (st) => {
          e.status = st;
          e.error = null;
        },
        (err) => {
          e.error = errorMessage(err);
        },
      )
      .finally(() => {
        e.loaded = true;
        e.inflight = null;
        notify();
      });
  }, [app, key, host, pane]);

  useEffect(() => {
    if (!key) return;
    const e = entry(key);
    const l = () => bump((n) => n + 1);
    e.listeners.add(l);
    return () => {
      e.listeners.delete(l);
    };
  }, [key]);

  useEffect(() => {
    if (!key || !active || !visible || !online) return;
    fetch();
    if (!poll) return;
    const id = setInterval(fetch, GIT_STATUS_REFRESH_MS);
    return () => clearInterval(id);
  }, [key, active, visible, online, poll, fetch]);

  // A finished turn usually means new edits: refresh right away (skip the first render).
  useEffect(() => {
    if (key && turns > 0 && active && visible && online) fetch();
  }, [turns]);

  const e = key ? cache.get(key) : undefined;
  return { status: e?.status ?? null, error: e?.error ?? null, loading: !!key && !e?.loaded, refresh: fetch };
}
