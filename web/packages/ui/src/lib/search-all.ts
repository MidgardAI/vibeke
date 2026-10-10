// "Search all agents": merge what every host found into one ranked list. `search.query` finds lines
// in pane scrollback (live and archived panes); `desk.search` finds turns of past and live harness
// sessions. Pure functions: the palette runs the requests (components/command-palette.tsx).

import { RpcError, type DeskHit, type SearchHit } from '@vibeke/core';

export interface SearchTarget {
  workspace: string | null;
  workspaceName: string | null;
  title: string | null;
  harness: string | null;
  /** A live pane the result can open (null for a past session without one). */
  pane: string | null;
}

/** Resolves ids from the live dashboards; unknown ids give null. */
export interface SearchLookup {
  pane(host: string, pane: string): SearchTarget | null;
  run(host: string, run: string): SearchTarget | null;
}

export interface SearchResult {
  /** Unique across hosts. */
  id: string;
  host: string;
  hostName: string;
  kind: 'scrollback' | 'session';
  title: string;
  workspaceName: string | null;
  harness: string | null;
  /** The matching text, one line. */
  snippet: string;
  /** Where to go: a live pane (null: nothing to open). */
  pane: string | null;
  workspace: string | null;
  run: string | null;
  /** 1-based line in the pane's scrollback, when the host said. */
  line: number | null;
  /** Milliseconds; 0 when unknown. */
  ts: number;
  live: boolean;
  score: number;
}

export interface HostHits {
  host: string;
  hostName: string;
  scrollback: readonly SearchHit[];
  desk: readonly DeskHit[];
}

/** Hosts give seconds or milliseconds; return milliseconds (0 for none). */
export function toMs(ts: number | null | undefined): number {
  if (typeof ts !== 'number' || !Number.isFinite(ts) || ts <= 0) return 0;
  return ts < 1e11 ? Math.round(ts * 1000) : Math.round(ts);
}

const oneLine = (s: string, max = 200): string => {
  const flat = s.replace(/\s+/g, ' ').trim();
  return flat.length > max ? `${flat.slice(0, max - 1)}…` : flat;
};

const DAY = 86_400_000;

/** How well `text` matches `query`: phrase, word start, terms present, short lines; 0 = not at all. */
export function matchScore(query: string, text: string): number {
  const q = query.trim().toLowerCase();
  if (!q) return 0;
  const hay = text.toLowerCase();
  let s = 0;
  const at = hay.indexOf(q);
  if (at >= 0) {
    s += 50;
    if (at === 0 || /\W/.test(hay[at - 1]!)) s += 15;
    if (at + q.length >= hay.length || /\W/.test(hay[at + q.length]!)) s += 5;
  }
  const terms = q.split(/\s+/).filter(Boolean);
  const present = terms.filter((w) => hay.includes(w)).length;
  s += terms.length ? (present / terms.length) * 20 : 0;
  s += Math.max(0, 10 - hay.length / 40);
  return s;
}

function recency(ts: number, now: number): number {
  if (!ts) return 0;
  return 20 * Math.exp(-Math.max(0, now - ts) / (7 * DAY));
}

/** Results per pane or session kept in the list, so one chatty pane does not fill it. */
export const PER_TARGET = 3;

/** Merge the hits of every host into ranked results (best first), at most `limit`. */
export function mergeResults(query: string, perHost: readonly HostHits[], lookup: SearchLookup, now: number, limit = 40): SearchResult[] {
  const byId = new Map<string, SearchResult>();
  for (const h of perHost) {
    for (const hit of h.scrollback) {
      const t = (hit.pane && lookup.pane(h.host, hit.pane)) || (hit.run ? lookup.run(h.host, hit.run) : null);
      const ts = toMs(hit.ts);
      const live = !!t?.pane;
      const snippet = oneLine(hit.text);
      const id = `s:${h.host}/${hit.pane ?? hit.pane_handle ?? '?'}/${hit.line ?? snippet}`;
      const r: SearchResult = {
        id,
        host: h.host,
        hostName: h.hostName,
        kind: 'scrollback',
        title: t?.title ?? hit.title ?? hit.pane_handle ?? hit.pane ?? '',
        workspaceName: t?.workspaceName ?? null,
        harness: t?.harness ?? null,
        snippet,
        pane: t?.pane ?? null,
        workspace: t?.workspace ?? hit.workspace ?? null,
        run: hit.run ?? null,
        line: typeof hit.line === 'number' && hit.line > 0 ? hit.line : null,
        ts,
        live,
        score: matchScore(query, snippet) + recency(ts, now) + (live ? 15 : 0),
      };
      if (!byId.has(id)) byId.set(id, r);
    }
    for (const hit of h.desk) {
      const liveRun = hit.live?.run ?? null;
      const livePane = hit.live?.pane ?? null;
      const t = (livePane && lookup.pane(h.host, livePane)) || (liveRun ? lookup.run(h.host, liveRun) : null);
      const ts = toMs(hit.ts);
      const snippet = oneLine(hit.snippet);
      const id = `d:${h.host}/${hit.session}/${hit.turn}`;
      const live = !!t?.pane;
      const where = hit.repo ?? hit.cwd ?? '';
      const r: SearchResult = {
        id,
        host: h.host,
        hostName: h.hostName,
        kind: 'session',
        title: t?.title ?? (where ? where.split('/').filter(Boolean).pop()! : hit.session),
        workspaceName: t?.workspaceName ?? null,
        harness: hit.harness || t?.harness || null,
        snippet,
        pane: t?.pane ?? null,
        workspace: t?.workspace ?? null,
        run: liveRun,
        line: null,
        ts,
        live,
        score: matchScore(query, snippet) + recency(ts, now) + (live ? 15 : 0),
      };
      if (!byId.has(id)) byId.set(id, r);
    }
  }
  const sorted = [...byId.values()].sort((a, b) => b.score - a.score || b.ts - a.ts || a.id.localeCompare(b.id));
  const perTarget = new Map<string, number>();
  const out: SearchResult[] = [];
  for (const r of sorted) {
    const target = `${r.host}/${r.pane ?? r.run ?? r.id}`;
    const n = perTarget.get(target) ?? 0;
    if (n >= PER_TARGET) continue;
    perTarget.set(target, n + 1);
    out.push(r);
    if (out.length >= limit) break;
  }
  return out;
}

export type SearchFailure = 'refused' | 'unsupported' | 'error';

/**
 * How a failed request reads: `refused` (a limited device asking for something host-wide) and
 * `unsupported` (an older gateway) are expected and not shown; anything else is a quiet note.
 */
export function classifySearchError(e: unknown): SearchFailure {
  if (e instanceof RpcError) {
    if (e.kind === 'forbidden' || e.kind === 'permission_denied') return 'refused';
    if (e.kind === 'method_not_found' || e.code === -32601) return 'unsupported';
  }
  return 'error';
}

/** Search needs at least this many characters. */
export const MIN_QUERY = 2;

export const searchableQuery = (q: string): string | null => {
  const s = q.trim();
  return s.length >= MIN_QUERY ? s : null;
};
