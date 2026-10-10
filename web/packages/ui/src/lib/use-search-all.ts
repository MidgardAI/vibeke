// "Search all agents": asks every online host in parallel (`search.query` for pane scrollback,
// `desk.search` for harness sessions), merges the answers as they arrive and drops stale ones.
// Refusals (a limited device) and old gateways are skipped quietly; real failures are listed.

import { useEffect, useMemo, useRef, useState } from 'react';
import { displayName, type DeskHit, type HostState, type SearchHit } from '@vibeke/core';
import { useApp, useHosts, useTree } from '../app/hooks';
import { classifySearchError, mergeResults, searchableQuery, type HostHits, type SearchLookup, type SearchResult, type SearchTarget } from './search-all';
import { rowTitle, type PaneTree } from './tree';

export const SEARCH_DEBOUNCE_MS = 250;
const REQUEST_TIMEOUT_MS = 15_000;

/** The host speaks the search methods of the gateway that added them. */
export const canSearch = (h: HostState): boolean => h.status === 'online' && !!h.info?.features.includes('search');

/** `desk.search` is host-wide: refused to limited devices and shares, so do not ask. */
export const canDeskSearch = (h: HostState): boolean => {
  const limit = h.info?.limit ?? h.record.limit;
  return !(limit && (limit.workspace || limit.pane)) && (h.info?.kind ?? h.record.kind ?? 'device') !== 'share';
};

export function lookupFromTree(tree: PaneTree): SearchLookup {
  const panes = new Map<string, SearchTarget>();
  const runs = new Map<string, SearchTarget>();
  for (const r of tree.all) {
    const target: SearchTarget = {
      workspace: r.pane.workspace,
      workspaceName: r.workspace ? displayName(r.workspace) : null,
      title: rowTitle(r),
      harness: r.run?.harness ?? null,
      pane: r.pane.id,
    };
    panes.set(`${r.host}/${r.pane.id}`, target);
    if (r.run) runs.set(`${r.host}/${r.run.id}`, target);
  }
  return { pane: (h, p) => panes.get(`${h}/${p}`) ?? null, run: (h, r) => runs.get(`${h}/${r}`) ?? null };
}

export interface SearchAllState {
  results: SearchResult[];
  /** Hosts still answering. */
  pending: number;
  /** Hosts asked. */
  asked: number;
  /** Names of hosts that failed in an unexpected way. */
  failed: string[];
  /** The query is long enough to search. */
  active: boolean;
}

const EMPTY: SearchAllState = { results: [], pending: 0, asked: 0, failed: [], active: false };

export function useSearchAll(query: string, enabled: boolean): SearchAllState {
  const app = useApp();
  const hosts = useHosts();
  const tree = useTree();
  const lookup = useMemo(() => lookupFromTree(tree), [tree]);
  const lookupRef = useRef(lookup);
  lookupRef.current = lookup;
  const hostsRef = useRef(hosts);
  hostsRef.current = hosts;
  const [hits, setHits] = useState<{ q: string; byHost: HostHits[]; pending: number; asked: number; failed: string[] } | null>(null);
  const q = enabled ? searchableQuery(query) : null;

  useEffect(() => {
    if (!q) {
      setHits(null);
      return;
    }
    let live = true;
    const targets = hostsRef.current.filter(canSearch);
    const byHost = new Map<string, HostHits>();
    const failed: string[] = [];
    let pending = targets.length;
    const publish = () => live && setHits({ q, byHost: [...byHost.values()], pending, asked: targets.length, failed: [...failed] });
    setHits({ q, byHost: [], pending, asked: targets.length, failed: [] });
    const timer = setTimeout(() => {
      for (const h of targets) {
        const id = h.record.host_id;
        const hostName = h.info?.host_name ?? h.record.name;
        const conn = app.conn(id);
        const scroll = conn
          ? conn.request('search.query', { q, sources: ['live', 'archive'], limit: 30, context: 0 }, { timeoutMs: REQUEST_TIMEOUT_MS })
          : Promise.reject(new Error('not connected'));
        const desk = conn && canDeskSearch(h) ? conn.request('desk.search', { text: q, limit: 30, sort: 'relevance' }, { timeoutMs: REQUEST_TIMEOUT_MS }) : Promise.resolve(null);
        void Promise.allSettled([scroll, desk]).then(([s, d]) => {
          if (!live) return;
          const bad = [s, d].filter((x): x is PromiseRejectedResult => x.status === 'rejected').filter((x) => classifySearchError(x.reason) === 'error');
          if (bad.length) failed.push(hostName);
          byHost.set(id, {
            host: id,
            hostName,
            scrollback: s.status === 'fulfilled' ? (s.value.hits as SearchHit[]) : [],
            desk: d.status === 'fulfilled' && d.value ? (d.value.hits as DeskHit[]) : [],
          });
          pending--;
          publish();
        });
      }
    }, SEARCH_DEBOUNCE_MS);
    return () => {
      live = false;
      clearTimeout(timer);
    };
  }, [app, q]);

  return useMemo(() => {
    if (!q) return EMPTY;
    if (!hits || hits.q !== q) return { ...EMPTY, active: true, pending: 1 };
    return {
      results: mergeResults(q, hits.byHost, lookup, app.platform.clock.now()),
      pending: hits.pending,
      asked: hits.asked,
      failed: hits.failed,
      active: true,
    };
  }, [q, hits, lookup, app]);
}
