// Who may use the assistant from the app, and the suggested-replies cache. The assistant is a
// host-wide feature: devices with a pane or workspace limit, shares, and view-only or approve-only
// devices do not get it. The host must also run a gateway that has the `catch_up` feature.

import type { AgentRun, HostInfo, HostRecord } from '@vibeke/core';

export interface AssistHostFacts {
  info: Pick<HostInfo, 'scope' | 'features' | 'kind' | 'limit'> | null;
  record: Pick<HostRecord, 'scope' | 'kind' | 'limit'>;
}

/** The device could ask the assistant (the host may still have it off or not configured). */
export function assistEligible(h: AssistHostFacts): boolean {
  const info = h.info;
  if (!info || !info.features.includes('catch_up')) return false;
  if ((info.scope ?? h.record.scope) !== 'full') return false;
  const limit = info.limit ?? h.record.limit;
  if (limit && (limit.workspace || limit.pane)) return false;
  return (info.kind ?? h.record.kind ?? 'device') !== 'share';
}

/** `assistant.status` -> usable (the host turned it on and configured a provider). */
export const assistReady = (s: { enabled?: unknown; configured?: unknown } | null | undefined): boolean => !!s && s.enabled === true && s.configured === true;

/** Changes after every finished turn: suggestions are valid until then. */
export const turnStamp = (run: Pick<AgentRun, 'turns_completed' | 'done_rev' | 'id'>): string => `${run.id}:${run.turns_completed}:${run.done_rev}`;

const MAX_ENTRIES = 50;

/** Suggested replies kept per pane until the run's next turn. */
export class ReplyCache {
  private m = new Map<string, { stamp: string; replies: string[] }>();

  get(key: string, stamp: string): string[] | null {
    const e = this.m.get(key);
    return e && e.stamp === stamp ? e.replies : null;
  }

  set(key: string, stamp: string, replies: string[]): void {
    this.m.delete(key);
    this.m.set(key, { stamp, replies });
    while (this.m.size > MAX_ENTRIES) this.m.delete(this.m.keys().next().value as string);
  }

  clear(): void {
    this.m.clear();
  }
}

export const replyCache = new ReplyCache();
