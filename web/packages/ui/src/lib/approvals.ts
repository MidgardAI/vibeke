// Approved calls (spec 09 §3.2 "Approved calls", spec 16 §15.2 "Sending from a pane"): a pane
// asks the user to run one specific call it may not make itself — send its work to another host,
// cancel one of its handoffs, redeem a peer invitation. The host freezes the call and summarises
// it from its own facts; the pane's reason is shown separately and never verified. The app lists
// open requests in the inbox (Approve / Deny) and on a review screen (the push opens it). Pure:
// app/approval-stores.ts and screens/approve.tsx build on it.

import type { AppApi, AppEvent, ApprovalDecision, ApprovalRequest } from '@vibeke/core';
import { t } from '../i18n';

export type DecideParams = AppApi['auth.approve.decide']['params'];
export type DecideResult = AppApi['auth.approve.decide']['result'];

/** What an event changes in a host's list. */
export type ApprovalChange = { k: 'upsert'; request: ApprovalRequest } | { k: 'remove'; id: string };

const str = (v: unknown): string => (typeof v === 'string' ? v : '');

/**
 * `auth.approval_requested` (subject {pane, request}, data {method, summary, reason, peer,
 * always_allowed}) adds a request; `granted`, `denied` and `withdrawn` end it. The event carries
 * the peer's id only, so the store refetches `auth.list` for the full record.
 */
export function approvalChange(e: AppEvent): ApprovalChange | null {
  const id = e.subject.request;
  if (!id) return null;
  switch (e.type) {
    case 'auth.approval_requested': {
      const d = e.data;
      const peer = str(d.peer);
      return {
        k: 'upsert',
        request: {
          request: id,
          kind: 'approval',
          pane: e.subject.pane ?? '',
          pane_handle: '',
          workspace: '',
          method: str(d.method),
          params: {},
          summary: str(d.summary),
          facts: {},
          reason: str(d.reason),
          reason_verified: false,
          peer: peer ? { id: peer, name: peer, owner: '' } : null,
          always_allowed: d.always_allowed === true,
          created_at_ms: e.ts || 0,
          status: 'pending',
        },
      };
    }
    case 'auth.approval_granted':
    case 'auth.approval_denied':
    case 'auth.approval_withdrawn':
      return { k: 'remove', id };
    default:
      return null;
  }
}

/** Apply a change; a fuller record (from `auth.list`) is never replaced by the event's thinner one. */
export function applyApproval(list: readonly ApprovalRequest[], c: ApprovalChange): ApprovalRequest[] {
  if (c.k === 'remove') return list.filter((r) => r.request !== c.id);
  const cur = list.find((r) => r.request === c.request.request);
  if (cur) return list.map((r) => (r === cur ? (cur.pane_handle ? cur : { ...c.request, created_at_ms: cur.created_at_ms || c.request.created_at_ms }) : r));
  return [...list, c.request];
}

/** Only requests still waiting for a decision, oldest first. */
export function openApprovals(list: readonly ApprovalRequest[]): ApprovalRequest[] {
  return list.filter((r) => (r.status ?? 'pending') === 'pending').sort((a, b) => a.created_at_ms - b.created_at_ms || a.request.localeCompare(b.request));
}

/**
 * Apply an `auth.list` snapshot issued at version `issued`: the snapshot's open requests, minus
 * every request seen ending (whenever), plus the requests events added after `issued` that the
 * snapshot doesn't have yet. Bookkeeping the snapshot settles is pruned.
 */
export function reconcileSnapshot(
  cur: readonly ApprovalRequest[],
  snapshot: readonly ApprovalRequest[],
  issued: number,
  removed: Map<string, number>,
  added: Map<string, number>,
): ApprovalRequest[] {
  const listed = new Set(snapshot.map((r) => r.request));
  let list = openApprovals(snapshot).filter((r) => !removed.has(r.request));
  for (const r of cur) {
    const at = added.get(r.request);
    if (at !== undefined && at > issued && !removed.has(r.request)) list = applyApproval(list, { k: 'upsert', request: r });
  }
  // Ended before the snapshot was issued and absent from it: the host agrees, forget it.
  for (const [id, at] of removed) if (at <= issued && !listed.has(id)) removed.delete(id);
  // Added before the snapshot was issued: the snapshot is the truth for it now.
  for (const [id, at] of added) if (at <= issued) added.delete(id);
  return openApprovals(list);
}

/** A running approved call may take a while (a peer redeem waits up to 60 s on the host). */
const DECIDE_TIMEOUT_MS = 90_000;

/** An older host without approved calls: nothing there. */
const unknownMethod = (e: unknown): boolean => e instanceof RpcError && (e.kind === 'method_not_found' || e.code === -32601);

/** The decisions offered for a request: `always` only when the host allows it (never for peer.redeem). */
export function decisionsFor(r: ApprovalRequest): ApprovalDecision[] {
  return r.always_allowed ? ['approve', 'always', 'deny'] : ['approve', 'deny'];
}

/** Params for `auth.approve.decide`; refuses `always` where the host would. */
export function decideParams(r: ApprovalRequest, decision: ApprovalDecision): DecideParams {
  if (!decisionsFor(r).includes(decision)) throw new Error(t.approve.onceOnly);
  return { request: r.request, decision };
}

/** What the call does, as a short phrase ("send a handoff"). */
export function approvalVerb(method: string): string {
  return t.approve.verbs[method] ?? t.approve.verbs.other!;
}

/** "Pane w1:p2 asks to send a handoff". */
export function approvalTitle(r: ApprovalRequest): string {
  return t.approve.title(r.pane_handle || r.pane, approvalVerb(r.method));
}

/** The toast after deciding: denied, approved (and its job), or approved but the call failed. */
export function decideOutcome(res: DecideResult): { tone: 'ok' | 'error' | 'info'; text: string } {
  if (res.decision === 'denied') return { tone: 'info', text: t.approve.denied };
  if (!res.ok) return { tone: 'error', text: t.approve.failed(res.error?.message ?? t.unknownError) };
  const r = (res.result ?? {}) as { job?: { id?: string }; peer?: { name?: string } };
  return { tone: 'ok', text: t.approve.approved(res.grant === 'always', r.job?.id ?? r.peer?.name ?? null) };
}
