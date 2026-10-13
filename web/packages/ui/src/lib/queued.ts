// Messages the agent's harness holds because they were sent while it worked (`agent.queued`).
// Claude Code queues them until it can take them; Codex keeps them until its next tool call.
// "Send now" (`agent.send_now`) makes the harness take them right away. Pure helpers here; the
// strip above the composer is `components/queued-inputs.tsx`.

import { RpcError, type AppEvent, type QueuedInput } from '@vibeke/core';

/** Events that may change the run's queue: the queue itself, and the run's state or turns. */
const TYPES = new Set(['agent.queued_changed', 'agent.state_changed', 'agent.turn_started', 'agent.turn_completed', 'agent.exited', 'agent.session_ended']);

export function isQueueEvent(e: AppEvent, run: string): boolean {
  return TYPES.has(e.type) && (e.subject ?? {}).run === run;
}

/** Oldest first; entries without an id are dropped (a broken answer shows nothing). */
export function queuedItems(r: unknown): QueuedInput[] {
  const items = (r as { items?: unknown } | null | undefined)?.items;
  if (!Array.isArray(items)) return [];
  return (items as QueuedInput[]).filter((i) => i && typeof i.id === 'string' && i.id).sort((a, b) => (a.since_ms ?? 0) - (b.since_ms ?? 0));
}

const reasonOf = (e: unknown): string | null => {
  if (!(e instanceof RpcError)) return null;
  const d = e.data?.details as { reason?: unknown } | undefined;
  return d && typeof d.reason === 'string' ? d.reason : null;
};

/** `agent.send_now` refused because the harness already took the messages. */
export const nothingQueued = (e: unknown): boolean => e instanceof RpcError && e.kind === 'conflict' && reasonOf(e) === 'nothing_queued';

/** The host does not have `agent.queued` (older hosts): stop asking. */
export const queueUnsupported = (e: unknown): boolean => e instanceof RpcError && (e.kind === 'method_not_found' || e.code === -32601);
