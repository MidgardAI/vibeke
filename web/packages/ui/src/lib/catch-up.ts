// "While you were away": what happened in each workspace since the user last looked. Pure
// functions over the workspace rows (lib/workspaces.ts) and a baseline of per-run marks saved when
// the app went to the background. No network and no React here; the store is lib/catch-up-store.ts.

import type { AgentRun, GitFile } from '@vibeke/core';
import { runKey } from './tree';
import type { WorkspaceRow } from './workspaces';

/** Backgrounded at least this long before it counts as "away". */
export const AWAY_MS = 5 * 60_000;

export interface RunMark {
  turns: number;
  done_rev: number;
}

/** What the user had seen at `at` (ms): one mark per `<host>/<run>`. */
export interface CatchUpBaseline {
  at: number;
  runs: Record<string, RunMark>;
  /** Workspaces dismissed at this time (ms): interactions opened before it no longer count. */
  seen?: Record<string, number>;
}

export interface CatchUpRun {
  /** `<host>/<run>` */
  key: string;
  run: AgentRun;
  /** Turns finished since the baseline. */
  turns: number;
  /** The run finished work (done_rev moved) since the baseline. */
  finished: boolean;
  /** The run started after the baseline. */
  isNew: boolean;
}

export interface CatchUpCard {
  /** `<host>/<workspace>` */
  key: string;
  host: string;
  hostName: string;
  workspace: string;
  title: string;
  branch: string | null;
  /** The task's review label as a short phrase ("ready for review"), when the host sent one. */
  checkLabel: string | null;
  /** The baseline time: "since" in the card. */
  since: number;
  /** Turns finished, summed over the runs. */
  turns: number;
  /** Interactions opened since the baseline that are still open. */
  waiting: number;
  runs: CatchUpRun[];
  /** The most recently active run's last message, shortened. */
  lastMessage: string | null;
  /** The pane to open and to ask `git.status` about. */
  pane: string | null;
}

export const isAway = (hiddenAt: number | null, now: number, awayMs = AWAY_MS): boolean => hiddenAt !== null && now - hiddenAt >= awayMs;

const marksOf = (rows: readonly WorkspaceRow[]): [string, AgentRun][] => rows.flatMap((w) => w.panes.flatMap((p) => (p.run ? [[runKey(w.host, p.run.id), p.run] as [string, AgentRun]] : [])));

/** Marks for every run in `rows`. */
export function snapshotRuns(rows: readonly WorkspaceRow[]): Record<string, RunMark> {
  const out: Record<string, RunMark> = {};
  for (const [k, r] of marksOf(rows)) out[k] = { turns: r.turns_completed, done_rev: r.done_rev };
  return out;
}

/** Flatten markdown-ish text to one short line (card excerpt). */
export function excerpt(text: string | null | undefined, max = 180): string | null {
  if (!text) return null;
  const flat = text
    .replace(/```[a-z0-9_-]*\n?/gi, ' ')
    .replace(/[`*]+/g, '')
    .replace(/^[#>\s]+/gm, '')
    .replace(/\s+/g, ' ')
    .trim();
  if (!flat) return null;
  if (flat.length <= max) return flat;
  const cut = flat.slice(0, max);
  const sp = cut.lastIndexOf(' ');
  return `${(sp > max * 0.6 ? cut.slice(0, sp) : cut).trimEnd()}…`;
}

/** The cards for workspaces with activity since `baseline`, most recently active first. */
export function buildCatchUp(rows: readonly WorkspaceRow[], baseline: CatchUpBaseline | null): CatchUpCard[] {
  if (!baseline) return [];
  const cards: { card: CatchUpCard; last: number }[] = [];
  for (const w of rows) {
    const runs: CatchUpRun[] = [];
    for (const p of w.panes) {
      const run = p.run;
      if (!run) continue;
      const key = runKey(w.host, run.id);
      const prev = baseline.runs[key];
      if (prev) {
        const turns = Math.max(0, run.turns_completed - prev.turns);
        const finished = run.done_rev > prev.done_rev;
        if (turns > 0 || finished) runs.push({ key, run, turns, finished, isNew: false });
      } else if (run.started_at_ms > baseline.at) {
        runs.push({ key, run, turns: run.turns_completed, finished: run.turns_completed > 0, isNew: true });
      }
    }
    const waitSince = Math.max(baseline.at, baseline.seen?.[w.key] ?? 0);
    const waiting = w.panes.reduce((n, p) => n + p.open.filter((i) => i.opened_at_ms > waitSince).length, 0);
    if (!runs.length && !waiting) continue;
    const latest = [...runs].sort((a, b) => b.run.execution.since_ms - a.run.execution.since_ms)[0];
    const msg = excerpt(latest?.run.last_message ?? w.summary);
    cards.push({
      last: Math.max(w.lastActivityMs, latest?.run.execution.since_ms ?? 0),
      card: {
        key: w.key,
        host: w.host,
        hostName: w.hostName,
        workspace: w.workspace.id,
        title: w.title,
        branch: w.branch,
        checkLabel: w.checkLabel,
        since: baseline.at,
        turns: runs.reduce((n, r) => n + r.turns, 0),
        waiting,
        runs,
        lastMessage: msg,
        pane: latest?.run.pane ?? w.primary?.pane.id ?? null,
      },
    });
  }
  return cards.sort((a, b) => b.last - a.last || a.card.key.localeCompare(b.card.key)).map((x) => x.card);
}

/** The baseline with `cards` marked seen at `now`: their runs get today's marks. */
export function markSeen(baseline: CatchUpBaseline, rows: readonly WorkspaceRow[], cards: readonly CatchUpCard[], now: number): CatchUpBaseline {
  const keys = new Set(cards.flatMap((c) => c.runs.map((r) => r.key)));
  const runs = { ...baseline.runs };
  for (const [k, r] of marksOf(rows)) if (keys.has(k)) runs[k] = { turns: r.turns_completed, done_rev: r.done_rev };
  const seen = { ...baseline.seen };
  for (const c of cards) seen[c.key] = now;
  return { at: baseline.at, runs, seen };
}

export interface ChangeTotals {
  files: number;
  adds: number;
  dels: number;
}

/** Files changed and added / removed lines from a `git.status` file list (binary and unknown counts as 0 lines). */
export function sumChanges(files: readonly Pick<GitFile, 'adds' | 'dels'>[]): ChangeTotals {
  let adds = 0;
  let dels = 0;
  for (const f of files) {
    adds += f.adds ?? 0;
    dels += f.dels ?? 0;
  }
  return { files: files.length, adds, dels };
}
