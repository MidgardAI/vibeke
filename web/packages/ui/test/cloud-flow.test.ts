import { describe, expect, test } from 'bun:test';
import type { AppEvent, CloudBox, CloudJob, Dashboard } from '@vibeke/core';
import { CloudStores } from '../src/app/cloud-stores';
import type { AppModel } from '../src/app/model';
import { CloudAuthQueue, adoptDestination, adoptNeedsRepo, cloudJobNeedsAuth, confirmedPruneParams, paneTaskKeys, sendBoxChoices, type CloudAuthRequest } from '../src/lib/cloud';

const box = (id: string, p: Partial<CloudBox> = {}): CloudBox => ({ box: `sprites/${id}`, provider: 'sprites', id, name: `vk-${id}`, state: 'running', ownership: 'attached', task: 't1', panes: [], unsynced: null, caps: {}, ...p });

describe('CloudAuthQueue', () => {
  const req = (id: number, host: string, provider: string, log: string[]): CloudAuthRequest => ({ id, host, provider, done: (ok) => log.push(`${id}:${ok}`) });

  test('shows one request at a time and ignores a stale completion', () => {
    const log: string[] = [];
    const q = new CloudAuthQueue();
    q.push(req(1, 'h1', 'sprites', log));
    q.push(req(2, 'h2', 'e2b', log));
    expect(q.current?.id).toBe(1);
    q.finish(1, false);
    expect(q.current?.id).toBe(2);
    // A late completion of request 1 must not dismiss request 2.
    q.finish(1, true);
    expect(q.current?.id).toBe(2);
    q.finish(2, true);
    expect(q.current).toBeNull();
    expect(log).toEqual(['1:false', '2:true']);
  });

  test('a sign-in settles queued requests for the same host and provider only', () => {
    const log: string[] = [];
    const q = new CloudAuthQueue();
    q.push(req(1, 'h1', 'sprites', log));
    q.push(req(2, 'h1', 'e2b', log));
    q.push(req(3, 'h1', 'sprites', log));
    q.finish(1, true);
    expect(log).toEqual(['1:true', '3:true']);
    expect(q.current?.id).toBe(2);
    q.clear();
    expect(log).toEqual(['1:true', '3:true', '2:false']);
  });
});

describe('send destinations', () => {
  const boxes = [box('mine'), box('other', { task: 't2' }), box('e', { provider: 'e2b', box: 'e2b/e' }), box('gone', { state: 'destroyed' }), box('orph', { ownership: 'orphaned' })];

  test('offers only the source task own box of the provider', () => {
    expect(sendBoxChoices(boxes, 'sprites', ['t1']).map((b) => b.id)).toEqual(['mine']);
  });

  test('offers no existing box when the task is unknown', () => {
    expect(sendBoxChoices(boxes, 'sprites', [])).toEqual([]);
  });

  test('finds the pane task as the host does: its workspace task only', () => {
    const d = {
      panes: [{ id: 'p1', handle: 'w1:p1', workspace: 'w1' }, { id: 'p2', handle: 'w2:p1', workspace: 'w2' }],
      runs: [{ pane: 'p1', task: 't9' }, { pane: 'p2', task: 't9' }],
      workspaces: [{ id: 'w1', task: 't1' }, { id: 'w2', task: null }],
      tasks: [{ id: 't1', handle: 'k1', slug: 'fix-it' }],
    } as unknown as Dashboard;
    // A run's own task never wins over the workspace task the host moves.
    expect(paneTaskKeys(d, 'p1')).toEqual(['t1', 'k1', 'fix-it']);
    // No workspace task: the host makes a new task (no box yet), so no existing box is offered.
    expect(paneTaskKeys(d, 'w2:p1')).toEqual([]);
    expect(sendBoxChoices([box('mine', { task: 't9' })], 'sprites', paneTaskKeys(d, 'w2:p1'))).toEqual([]);
    expect(paneTaskKeys(d, 'nope')).toEqual([]);
    expect(paneTaskKeys(null, 'p1')).toEqual([]);
  });
});

describe('clean up and adopt', () => {
  test('the confirmed prune names the previewed boxes', () => {
    expect(confirmedPruneParams([box('a'), box('b')])).toEqual({ ownership: ['orphaned', 'idle'], boxes: ['sprites/a', 'sprites/b'] });
  });

  test('adopt opens the first returned pane in its workspace', () => {
    const r = { box: 'sprites/a', task: 't7', panes: ['p8', 'p9'], sessions: 2, repo: '/r', branch: 'b', worktree: '/w' };
    const d = { panes: [{ id: 'p8', workspace: 'w8' }], tasks: [{ id: 't7', handle: 'k7', workspace: 'w7' }] } as unknown as Dashboard;
    expect(adoptDestination(r, d)).toEqual({ pane: 'p8', workspace: 'w8' });
    // Before the dashboard has the pane: the task's workspace, else the pane alone.
    expect(adoptDestination(r, { panes: [], tasks: d.tasks } as unknown as Dashboard)).toEqual({ pane: 'p8', workspace: 'w7' });
    expect(adoptDestination(r, null)).toEqual({ pane: 'p8', workspace: null });
    expect(adoptDestination({ task: 't7', panes: [] }, null)).toEqual({ pane: null, workspace: null });
  });

  test('adopt asks for a repository on invalid_params about the repo', () => {
    expect(adoptNeedsRepo({ kind: 'invalid_params', message: 'cloud.box.adopt: the box has no repo; pass repo' })).toBe(true);
    expect(adoptNeedsRepo({ kind: 'invalid_params', message: 'unknown box' })).toBe(false);
    expect(adoptNeedsRepo({ kind: 'conflict', message: 'repo busy' })).toBe(false);
  });
});

const job = (id: string, p: Partial<CloudJob> = {}): CloudJob => ({ id, direction: 'send', state: 'uploading', created_at: 1, updated_at: 1, ...p });

describe('job failures', () => {
  test('needs_auth in the job error asks for a sign-in', () => {
    const j = job('j', { state: 'failed', error: { kind: 'permission_denied', message: 'revoked', details: { reason: 'needs_auth', provider: 'sprites', methods: [{ kind: 'env', var: 'X' }] } } });
    expect(cloudJobNeedsAuth(j)).toEqual({ provider: 'sprites', methods: [{ kind: 'env', var: 'X' }] });
    expect(cloudJobNeedsAuth({ ...j, state: 'uploading' })).toBeNull();
    expect(cloudJobNeedsAuth(job('k', { state: 'failed', error: { message: 'boom' } }))).toBeNull();
  });
});

describe('CloudStores', () => {
  const fakeApp = () => {
    const listeners = new Map<string, (e: AppEvent) => void>();
    const toasts: string[] = [];
    let subscribed = 0;
    const host = { record: { host_id: 'h1', name: 'h', scope: 'full' }, info: { scope: 'full' }, status: 'offline' };
    const app = {
      manager: {
        subscribe: () => (subscribed++, () => subscribed--),
        getSnapshot: () => [host],
        subscribeEvents: (id: string, cb: (e: AppEvent) => void) => (listeners.set(id, cb), () => listeners.delete(id)),
      },
      conn: () => undefined,
      toast: (m: string) => toasts.push(m),
    } as unknown as AppModel;
    const emit = (j: CloudJob) => listeners.get('h1')?.({ seq: 1, ts: 1, type: 'cloud.job', subject: { job: j.id }, data: j as unknown as Record<string, unknown> });
    return { app, toasts, emit, live: () => subscribed > 0 && listeners.size > 0 };
  };

  test('a tracked job keeps the store live until it ends, then toasts', () => {
    const f = fakeApp();
    const s = new CloudStores(f.app);
    expect(f.live()).toBe(false);
    s.trackJob('h1', job('j1'));
    expect(f.live()).toBe(true);
    f.emit(job('j1', { state: 'done', updated_at: 2 }));
    expect(f.toasts).toEqual(['Moved.']);
    expect(f.live()).toBe(false);
  });

  test('a job a sheet shows ends without a toast and still releases the store', () => {
    const f = fakeApp();
    const s = new CloudStores(f.app);
    s.trackJob('h1', job('j2'));
    const unwatch = s.watch('h1', 'j2');
    f.emit(job('j2', { state: 'failed', updated_at: 2, error: { message: 'x' } }));
    unwatch();
    expect(f.toasts).toEqual([]);
    expect(f.live()).toBe(false);
  });
});

import { RpcError as RpcErr } from '@vibeke/core';
import { parseCloudJob as parseJob, parseReview, sharedCheckout } from '../src/lib/cloud';

describe('bring-back review', () => {
  test('a job waiting for review carries its summary', () => {
    const j = parseJob({
      id: 'j1',
      direction: 'bring_back',
      state: 'awaiting_confirmation',
      result: {
        review: {
          warning: 'The agent ran with full permissions.',
          files: [{ path: 'src/a.ts', added: 3, removed: 1 }, { nope: true }],
          files_total: 40,
          added: 120,
          removed: 9,
          untracked: ['notes.md', 7],
          untracked_total: 1,
          skipped: [{ path: 'big.bin', reason: 'too large' }],
          transcript: true,
        },
      },
    })!;
    expect(j.state).toBe('awaiting_confirmation');
    expect(j.result?.review).toMatchObject({
      warning: 'The agent ran with full permissions.',
      files: [{ path: 'src/a.ts', added: 3, removed: 1 }],
      files_total: 40,
      untracked: ['notes.md'],
      skipped: [{ path: 'big.bin', reason: 'too large' }],
      transcript: true,
    });
    expect(parseReview(null)).toBeNull();
  });
  test('a shared checkout refusal offers a new task', () => {
    const e = new RpcErr('cloud.move', { code: -32009, message: 'Pane w1:p1 works in the main checkout.', data: { kind: 'conflict', details: { reason: 'shared_checkout', retry: { new_task: true } } } });
    expect(sharedCheckout(e)).toEqual({ message: 'Pane w1:p1 works in the main checkout.' });
    expect(sharedCheckout(new Error('x'))).toBeNull();
  });
});
