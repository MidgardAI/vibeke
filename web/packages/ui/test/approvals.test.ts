import { describe, expect, test } from 'bun:test';
import { MUTATING_METHODS, type AppEvent, type ApprovalRequest } from '@vibeke/core';
import { ApprovalStores } from '../src/app/approval-stores';
import { applyApproval, approvalChange, approvalTitle, decideOutcome, decideParams, decisionsFor, openApprovals, reconcileSnapshot } from '../src/lib/approvals';
import { formatRoute, parseRoute } from '../src/router';
import { host } from './fixtures';

const SUMMARY = 'Send pane w1:p2 (repo api, branch main, 3 changed files, agent: none) to marvin (your host)';

const request = (p: Partial<ApprovalRequest> = {}): ApprovalRequest => ({
  request: 'ap-1',
  kind: 'approval',
  pane: 'p2',
  pane_handle: 'w1:p2',
  workspace: 'W1',
  method: 'handoff.send',
  params: { pane: 'p2', peer: 'pe-1', interrupt: false },
  summary: SUMMARY,
  facts: {},
  reason: 'ship it',
  reason_verified: false,
  peer: { id: 'pe-1', name: 'marvin', owner: 'self' },
  always_allowed: true,
  created_at_ms: 100,
  status: 'pending',
  ...p,
});

const event = (type: string, data: Record<string, unknown>, request = 'ap-1'): AppEvent => ({ seq: 1, ts: 200, type, subject: { pane: 'p2', request }, data });

describe('approved calls (spec 09 §3.2)', () => {
  test('a requested event adds a pending request with the host summary and the unverified reason', () => {
    const c = approvalChange(event('auth.approval_requested', { method: 'handoff.send', summary: SUMMARY, reason: 'ship it', peer: 'pe-1', always_allowed: true }));
    expect(c?.k).toBe('upsert');
    const list = applyApproval([], c!);
    expect(list).toHaveLength(1);
    expect(list[0]).toMatchObject({ request: 'ap-1', pane: 'p2', summary: SUMMARY, reason: 'ship it', reason_verified: false, always_allowed: true, status: 'pending', created_at_ms: 200 });
    expect(list[0]!.peer?.id).toBe('pe-1');
    // always_allowed must be literally true.
    const c2 = approvalChange(event('auth.approval_requested', { method: 'gateway.call', summary: 'Redeem', always_allowed: 'yes' }, 'ap-2'));
    expect(c2?.k === 'upsert' && c2.request.always_allowed).toBe(false);
  });

  test('granted, denied and withdrawn end a request; other events and events without a request are ignored', () => {
    const list = [request(), request({ request: 'ap-2' }), request({ request: 'ap-3' })];
    let out = applyApproval(list, approvalChange(event('auth.approval_granted', { grant: 'once', ok: true }))!);
    out = applyApproval(out, approvalChange(event('auth.approval_denied', {}, 'ap-2'))!);
    expect(out.map((r) => r.request)).toEqual(['ap-3']);
    out = applyApproval(out, approvalChange(event('auth.approval_withdrawn', { reason: 'disconnected' }, 'ap-3'))!);
    expect(out).toEqual([]);
    expect(approvalChange(event('auth.elevate_requested', {}))).toBeNull();
    expect(approvalChange({ ...event('auth.approval_requested', {}), subject: { pane: 'p2' } })).toBeNull();
  });

  test('a listed (full) record is never replaced by the thinner event copy', () => {
    const full = request();
    const thin = approvalChange(event('auth.approval_requested', { method: 'handoff.send', summary: 'x', peer: 'pe-1' }))!;
    expect(applyApproval([full], thin)).toEqual([full]);
  });

  test('only pending requests are open, oldest first', () => {
    const out = openApprovals([request({ request: 'b', created_at_ms: 3 }), request({ request: 'a', created_at_ms: 1 }), request({ request: 'c', status: 'running' })]);
    expect(out.map((r) => r.request)).toEqual(['a', 'b']);
  });

  test('decide params are exact; always only where the host allows it', () => {
    expect(decideParams(request(), 'approve')).toEqual({ request: 'ap-1', decision: 'approve' });
    expect(decideParams(request(), 'always')).toEqual({ request: 'ap-1', decision: 'always' });
    expect(decideParams(request(), 'deny')).toEqual({ request: 'ap-1', decision: 'deny' });
    const redeem = request({ method: 'gateway.call', always_allowed: false, peer: null });
    expect(decisionsFor(redeem)).toEqual(['approve', 'deny']);
    expect(decisionsFor(request())).toEqual(['approve', 'always', 'deny']);
    expect(() => decideParams(redeem, 'always')).toThrow();
    // Deciding carries an op_id (retries never run the approved call twice).
    expect(MUTATING_METHODS.has('auth.approve.decide')).toBe(true);
    expect(MUTATING_METHODS.has('auth.list')).toBe(false);
  });

  test('titles and outcomes', () => {
    expect(approvalTitle(request())).toBe('Pane w1:p2 asks to send a handoff');
    expect(approvalTitle(request({ pane_handle: '', method: 'gateway.call' }))).toBe('Pane p2 asks to redeem a peer invitation');
    expect(approvalTitle(request({ method: 'x.y' }))).toBe('Pane w1:p2 asks to run a call');
    const base = { request: 'ap-1', pane: 'p2', error: null };
    expect(decideOutcome({ ...base, decision: 'denied', grant: null, ok: false, result: null })).toEqual({ tone: 'info', text: 'Denied' });
    expect(decideOutcome({ ...base, decision: 'approved', grant: 'once', ok: true, result: { job: { id: 'job-7' } } })).toEqual({ tone: 'ok', text: 'Approved: job-7' });
    expect(decideOutcome({ ...base, decision: 'approved', grant: 'always', ok: true, result: { job: { id: 'job-8' } } }).text).toBe('Approved (always for this pane): job-8');
    const failed = decideOutcome({ ...base, decision: 'approved', grant: 'once', ok: false, result: null, error: { message: 'repo_moved: the pane moved' } });
    expect(failed.tone).toBe('error');
    expect(failed.text).toContain('repo_moved');
  });

  test('review routes (where the push notification lands)', () => {
    expect(parseRoute('#/approve')).toEqual({ name: 'approve', host: null, id: null });
    expect(parseRoute('#/approve/h1')).toEqual({ name: 'approve', host: 'h1', id: null });
    expect(parseRoute('#/approve/h1/01JAPPROVE')).toEqual({ name: 'approve', host: 'h1', id: '01JAPPROVE' });
    expect(formatRoute({ name: 'approve', host: 'h1', id: '01JAPPROVE' })).toBe('#/approve/h1/01JAPPROVE');
    expect(formatRoute({ name: 'approve', host: 'h1', id: null })).toBe('#/approve/h1');
    expect(formatRoute({ name: 'approve', host: null, id: null })).toBe('#/approve');
  });

  test('a snapshot never resurrects a request seen ending and keeps requests added after it was issued', () => {
    const removed = new Map<string, number>([['gone', 2]]);
    const added = new Map<string, number>([['old', 1], ['new', 3]]);
    const cur = [request({ request: 'old', pane_handle: '' }), request({ request: 'new', pane_handle: '', created_at_ms: 300 })];
    // Issued at version 1: 'gone' ended (v2) and 'new' arrived (v3) while it was out.
    const out = reconcileSnapshot(cur, [request({ request: 'gone' }), request({ request: 'listed', created_at_ms: 50 })], 1, removed, added);
    expect(out.map((r) => r.request)).toEqual(['listed', 'new']);
    // 'old' (added at v1, before the snapshot was issued) is gone on the host: dropped, settled.
    expect(added.has('old')).toBe(false);
    expect(added.has('new')).toBe(true);
    // 'gone' ended after the snapshot was issued: still remembered for the next one.
    expect(removed.has('gone')).toBe(true);
    // A snapshot issued after both settles them: the full record replaces the event's copy.
    const next = reconcileSnapshot(out, [request({ request: 'new', created_at_ms: 300 })], 3, removed, added);
    expect(next.map((r) => r.request)).toEqual(['new']);
    expect(next[0]!.pane_handle).toBe('w1:p2');
    expect(removed.size).toBe(0);
    expect(added.size).toBe(0);
  });

  test('refreshes are serialized and coalesced; a withdrawal seen while one is out wins', async () => {
    const calls: { method: string; resolve: (v: unknown) => void }[] = [];
    const h = host('h1', null);
    const conn = {
      getSnapshot: () => h,
      request: (method: string) => new Promise((resolve) => calls.push({ method, resolve })),
    };
    let onEvent: ((e: AppEvent) => void) | null = null;
    const manager = {
      getSnapshot: () => [h],
      subscribe: () => () => {},
      subscribeEvents: (_id: string, cb: (e: AppEvent) => void) => {
        onEvent = cb;
        return () => {};
      },
    };
    const app = { manager, conn: () => conn, haptic: () => {}, toast: () => {} };
    const stores = new ApprovalStores(app as never);
    const stop = stores.start();
    const flush = () => new Promise((r) => setTimeout(r, 0));
    const ids = () => (stores.hosts.get().get('h1')?.list ?? []).map((r) => r.request);
    const emit = (type: string, id: string) => onEvent!(event(type, { method: 'handoff.send', summary: SUMMARY, peer: 'pe-1', always_allowed: true }, id));
    // The connect refresh is out; events arrive meanwhile.
    expect(calls.map((c) => c.method)).toEqual(['auth.list']);
    emit('auth.approval_requested', 'ap-1');
    emit('auth.approval_requested', 'ap-2');
    // Coalesced: no second auth.list while the first is in flight.
    expect(calls).toHaveLength(1);
    expect(ids()).toEqual(['ap-1', 'ap-2']);
    emit('auth.approval_withdrawn', 'ap-1');
    expect(ids()).toEqual(['ap-2']);
    // The older snapshot still lists ap-1 and lacks ap-2.
    calls[0]!.resolve({ approvals: [request({ request: 'ap-1' })], grants: [] });
    await flush();
    expect(ids()).toEqual(['ap-2']);
    // One follow-up for both events, issued after the first answer landed.
    expect(calls).toHaveLength(2);
    calls[1]!.resolve({ approvals: [request({ request: 'ap-2' })], grants: [] });
    await flush();
    expect(ids()).toEqual(['ap-2']);
    expect(stores.hosts.get().get('h1')!.list[0]!.pane_handle).toBe('w1:p2');
    expect(calls).toHaveLength(2);
    stop();
  });
});
