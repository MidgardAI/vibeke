import { describe, expect, test } from 'bun:test';
import { MUTATING_METHODS, type AppEvent, type ApprovalRequest } from '@vibeke/core';
import { applyApproval, approvalChange, approvalTitle, decideOutcome, decideParams, decisionsFor, openApprovals } from '../src/lib/approvals';
import { formatRoute, parseRoute } from '../src/router';

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
});
