// Work the agent harness keeps running after its turn ended (`AgentRun.bg_tasks`): background
// shells, subagents, monitors. One place derives what the status chips, rows and the chat strip show.

import type { AgentRun, BgTask } from '@vibeke/core';
import { t } from '../i18n';

/** The run's background tasks, oldest first. Older servers send none. */
export function bgTasksOf(run: AgentRun | null | undefined): BgTask[] {
  const list = run?.bg_tasks;
  if (!list || !list.length) return [];
  return [...list].sort((a, b) => a.since_ms - b.since_ms);
}

/** Is the agent busy itself (its own turn runs)? */
export const runWorking = (run: AgentRun | null | undefined): boolean => run?.execution.value === 'working' || run?.execution.value === 'starting';

/**
 * Background work to call out: the run has tasks and is not working on a turn (while it works,
 * the normal working state already says so). Ended or failed runs have nothing to show.
 */
export function bgCount(run: AgentRun | null | undefined): number {
  if (!run || runWorking(run)) return 0;
  const v = run.execution.value;
  if (v === 'error' || v === 'exited') return 0;
  return bgTasksOf(run).length;
}

/** "2 in background" for a run with background work and no turn running; null otherwise. */
export function bgSummary(run: AgentRun | null | undefined): string | null {
  const n = bgCount(run);
  return n > 0 ? t.bg.count(n) : null;
}

/** Tool call ids of the run's background tasks. */
export function bgToolIds(run: AgentRun | null | undefined): Set<string> {
  const ids = new Set<string>();
  for (const b of bgTasksOf(run)) if (b.tool_use_id) ids.add(b.tool_use_id);
  return ids;
}
