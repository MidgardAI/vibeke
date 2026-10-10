// Warm the data a destination needs when a finger lands on a row, so the screen opens with it.
// Reads only: nothing here marks a run as seen (that happens in the workspace screen).

import type { AppApi, HostConnectionApi, StyledScreen } from '@vibeke/core';
import type { CachedMirror } from '../platform';
import { styledLines } from './styled';

/** Same size as the first page the conversation asks for (screens/workspace/conversation.tsx). */
export const TRANSCRIPT_FIRST_PAGE = 20;
const MIRROR_LINES = 400;
/** A warm result is used if the screen opens within this time. */
export const PREFETCH_TTL_MS = 20_000;
/** Do not start the same prefetch again within this time. */
export const PREFETCH_COOLDOWN_MS = 8_000;

export type Transcript = AppApi['agent.transcript']['result'];

/** Pure throttle: true (and records the time) when `key` may start a prefetch at `nowMs`. */
export function claimPrefetch(last: Map<string, number>, key: string, nowMs: number, cooldownMs = PREFETCH_COOLDOWN_MS): boolean {
  const at = last.get(key);
  if (at !== undefined && nowMs - at < cooldownMs) return false;
  last.set(key, nowMs);
  if (last.size > 200) last.delete(last.keys().next().value!);
  return true;
}

const lastStart = new Map<string, number>();
const transcripts = new Map<string, { at: number; value: Transcript }>();

/** A prefetched first transcript page for a run, if fresh. It is handed out once. */
export function takePrefetchedTranscript(host: string, run: string, nowMs: number): Transcript | null {
  const k = `${host}/${run}`;
  const v = transcripts.get(k);
  transcripts.delete(k);
  return v && nowMs - v.at <= PREFETCH_TTL_MS ? v.value : null;
}

export interface PrefetchTarget {
  host: string;
  pane: string;
  /** The pane's live agent run, if any (its transcript is what the screen shows first). */
  run: string | null;
  /** Fetch the terminal screen: shells, and agents shown as a terminal. */
  mirror: boolean;
}

interface PrefetchEnv {
  conn(host: string): HostConnectionApi | undefined;
  now(): number;
  mirrors: Map<string, CachedMirror>;
  saveMirror?(key: string, v: CachedMirror): Promise<void>;
}

export function prefetchTarget(env: PrefetchEnv, t: PrefetchTarget): void {
  const conn = env.conn(t.host);
  if (!conn || conn.getSnapshot().status !== 'online') return;
  const now = env.now();
  if (t.run && claimPrefetch(lastStart, `t/${t.host}/${t.run}`, now)) {
    const run = t.run;
    conn.request('agent.transcript', { target: run, limit: TRANSCRIPT_FIRST_PAGE }).then(
      (value) => {
        transcripts.set(`${t.host}/${run}`, { at: env.now(), value });
        if (transcripts.size > 20) transcripts.delete(transcripts.keys().next().value!);
      },
      () => {},
    );
  }
  if (t.mirror && claimPrefetch(lastStart, `m/${t.host}/${t.pane}`, now)) {
    const key = `${t.host}/${t.pane}`;
    conn.request('pane.read', { pane: t.pane, source: 'styled', lines: MIRROR_LINES }).then(
      (r) => {
        if (!r || !('rows' in r) || !Array.isArray((r as StyledScreen).rows)) return;
        const { lines, text } = styledLines((r as StyledScreen).rows);
        const v: CachedMirror = { text, at: env.now(), lines };
        env.mirrors.set(key, v);
        void env.saveMirror?.(key, v).catch(() => {});
      },
      () => {},
    );
  }
}
