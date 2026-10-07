import { describe, expect, test } from 'bun:test';
import type { AppEvent, HandoffJob, IncomingHandoff, PeerInfo } from '@vibeke/core';
import { findIncomingForJob, jobFromEvent, jobView, mergePeers, parseJob, planSend, sendDestinations, upsertJob, type KnownPeer } from '../src/lib/handoff-send';
import {
  acceptParams,
  actionCount,
  applyIncoming,
  cleanPath,
  importedTarget,
  incomingChange,
  initialForm,
  repoChoices,
  repoMismatch,
  sortIncoming,
  validBranch,
  type AcceptForm,
} from '../src/lib/incoming';
import { formatRoute, parseRoute } from '../src/router';
import { host } from './fixtures';

const peerInfo = (id: string, hostId: string, p: Partial<PeerInfo> = {}): PeerInfo => ({
  id,
  name: id,
  relay: 'wss://r',
  host: hostId,
  device_id: 'd',
  owner: 'self',
  added_at: 0,
  expires_at: null,
  expired: false,
  ...p,
});

const job = (p: Partial<HandoffJob> = {}): HandoffJob => ({ id: 'j1', pane: 'p1', peer: 'pe1', peer_name: 'laptop', state: 'queued', sent: 0, total: 0, interrupt: false, created_at: 1, updated_at: 1, ...p });

const incoming = (p: Partial<IncomingHandoff> & { branch?: string | null } = {}): IncomingHandoff => {
  const { branch, ...rest } = p;
  return {
    id: 'in1',
    from: { host: 'devbox', owner: 'self' },
    manifest: {
      source_host: 'devbox',
      repo_name: 'samplehub',
      origin: 'git@github.com:a/samplehub.git',
      branch: branch === undefined ? 'feat/x' : branch,
      head: 'abc',
      harness: 'claude',
      session_id: 's1',
      cwd_rel: '',
      skipped: [
        { path: '.env', reason: 'secret' },
        { path: 'big.bin', reason: 'larger than 5 MiB' },
      ],
      last_message: 'done',
      untracked: 1,
      transcript: true,
      redactions: 0,
      created_at: 0,
    },
    size: 1000,
    bundle_path: '/s/in1.tar.zst',
    state: 'pending',
    error: null,
    result: null,
    created_at_ms: 1000,
    updated_at_ms: 1000,
    expires_at_ms: 9_000_000,
    ...rest,
  };
};

const event = (type: string, data: Record<string, unknown>, subject: Record<string, string> = {}): AppEvent => ({ seq: 1, ts: 0, type, subject, data });

describe('send destinations', () => {
  const src = host('src', null);
  const laptop = host('laptop', null);
  const mini = host('mini', null);
  const offline = host('old-mac', null, 'offline');
  const viewOnly = { ...host('view', null), info: null, record: { ...host('view', null).record, scope: 'view' as const } };
  const share = { ...host('sh', null), record: { ...host('sh', null).record, kind: 'share' as const, scope: 'approve' as const } };

  test('peers of the source come first (own, then teammates), then own hosts that are not peers yet', () => {
    const peers: KnownPeer[] = [
      { id: 'p-team', name: 'anna-box', owner: 'teammate', expires_at: 2000, host: 'anna' },
      { id: 'p-lap', name: 'laptop', owner: 'self', expires_at: null, host: 'laptop' },
    ];
    const d = sendDestinations('src', peers, [src, laptop, mini, offline, viewOnly, share], 1000);
    expect(d.map((x) => x.key)).toEqual(['peer:p-lap', 'peer:p-team', 'host:mini', 'host:old-mac']);
    expect(d[0]).toMatchObject({ peer: 'p-lap', hostId: 'laptop', owner: 'self', expired: false });
    expect(d[1]).toMatchObject({ peer: 'p-team', hostId: null, owner: 'teammate', expiresAt: 2000, expired: false });
    expect(d[2]).toMatchObject({ peer: null, hostId: 'mini', online: true });
    expect(d[3]).toMatchObject({ peer: null, hostId: 'old-mac', online: false });
  });

  test('expired teammate peers are listed but cannot be used', () => {
    const d = sendDestinations('src', [{ id: 'p', name: 'anna', owner: 'teammate', expires_at: 500 }], [src], 1000);
    expect(d[0]!.expired).toBe(true);
    expect(planSend(d[0]!)).toEqual({ k: 'unavailable', reason: 'expired' });
  });

  test('auto-pair only for own hosts the app reaches now', () => {
    const d = sendDestinations('src', [{ id: 'p-lap', name: 'laptop', owner: 'self', expires_at: null, host: 'laptop' }], [src, laptop, mini, offline], 0);
    const by = (k: string) => d.find((x) => x.key === k)!;
    expect(planSend(by('peer:p-lap'))).toEqual({ k: 'send', peer: 'p-lap' });
    expect(planSend(by('host:mini'))).toEqual({ k: 'pair', hostId: 'mini' });
    expect(planSend(by('host:old-mac'))).toEqual({ k: 'unavailable', reason: 'offline' });
  });

  test('handoff.peers is annotated with peer.list host ids; either list may be missing', () => {
    const list = [peerInfo('p1', 'laptop', { name: 'laptop' }), peerInfo('p2', 'anna', { name: 'anna', owner: 'teammate', expires_at: 50, expired: true })];
    expect(mergePeers([{ id: 'p1', name: 'laptop', owner: 'self' }], list)).toEqual([{ id: 'p1', name: 'laptop', owner: 'self', expires_at: null, expired: false, host: 'laptop' }]);
    expect(mergePeers(null, list).map((p) => [p.id, p.host, p.expired])).toEqual([
      ['p1', 'laptop', false],
      ['p2', 'anna', true],
    ]);
    expect(mergePeers([{ id: 'p9', name: 'x', owner: 'self', expires_at: 7 }], null)).toEqual([{ id: 'p9', name: 'x', owner: 'self', expires_at: 7 }]);
  });
});

describe('handoff jobs', () => {
  test('parses jobs from results and events, ignoring junk', () => {
    expect(parseJob(null)).toBeNull();
    expect(parseJob({ id: 'x', state: 'teleporting' })).toBeNull();
    expect(parseJob({ id: 'x', state: 'sending', sent: -1, total: 'big', peer: 'p' })).toMatchObject({ sent: 0, total: 0, peer_name: 'p', incoming_state: null, error: null });
    expect(jobFromEvent(event('handoff.job', { id: 'j1', state: 'sending', sent: 5, total: 10, peer: 'p', peer_name: 'laptop', pane: 'p1' }))).toMatchObject({ id: 'j1', sent: 5 });
    expect(jobFromEvent(event('handoff.job', { job: { state: 'queued', peer: 'p', pane: 'p1' } }, { job: 'j2' }))).toMatchObject({ id: 'j2', state: 'queued' });
    expect(jobFromEvent(event('handoff.updated', { id: 'j1', state: 'sending' }))).toBeNull();
  });

  test('progress and outcome phases', () => {
    expect(jobView(job({ state: 'queued' }))).toMatchObject({ phase: 'queued', pct: null });
    expect(jobView(job({ state: 'sending', sent: 0, total: 0 }))).toMatchObject({ phase: 'sending', pct: 0 });
    expect(jobView(job({ state: 'sending', sent: 512, total: 1024 }))).toMatchObject({ phase: 'sending', pct: 50 });
    expect(jobView(job({ state: 'sending', sent: 2048, total: 1024 })).pct).toBe(100);
    expect(jobView(job({ state: 'delivered', incoming_state: 'pending' })).phase).toBe('pending');
    expect(jobView(job({ state: 'delivered', incoming_state: 'imported' })).phase).toBe('imported');
    expect(jobView(job({ state: 'delivered', incoming_state: 'failed' })).phase).toBe('import_failed');
    expect(jobView(job({ state: 'delivered' })).phase).toBe('delivered');
    expect(jobView(job({ state: 'failed', error: { kind: 'unavailable', message: 'peer offline' } }))).toMatchObject({ phase: 'failed', error: 'peer offline' });
    expect(jobView(job({ state: 'failed', error: 'boom' })).error).toBe('boom');
  });

  test('newer copies win; stale updates are ignored', () => {
    const a = upsertJob([], job({ state: 'sending', sent: 10, updated_at: 5 }));
    const b = upsertJob(a, job({ state: 'sending', sent: 5, updated_at: 4 }));
    expect(b[0]!.sent).toBe(10);
    const c = upsertJob(b, job({ state: 'delivered', sent: 20, updated_at: 6 }));
    expect(c).toHaveLength(1);
    expect(c[0]!.state).toBe('delivered');
    expect(upsertJob(c, job({ id: 'j2' }))).toHaveLength(2);
  });

  test('finds the record a delivered job became on an own host', () => {
    const list = [
      incoming({ id: 'old', created_at_ms: 1_000 }),
      incoming({ id: 'other', from: { host: 'mini', owner: 'self' }, created_at_ms: 500_000 }),
      incoming({ id: 'new', created_at_ms: 400_000 }),
    ];
    expect(findIncomingForJob(list, 'devbox', 450_000)?.id).toBe('new');
    expect(findIncomingForJob(list, 'devbox', 450_000, 'main')).toBeNull();
    expect(findIncomingForJob(list, 'devbox', 900_000)).toBeNull();
  });
});

describe('accepting an incoming handoff', () => {
  const suggested = { repos: ['/home/u/code/samplehub'], repo: '/home/u/code/samplehub', worktree_path: '/home/u/code/samplehub-handoff-feat-x', branch: 'feat/x' };
  const form = (p: Partial<AcceptForm> = {}): AcceptForm => ({ ...initialForm(incoming(), suggested), ...p });

  test('defaults come from the host: the matching clone, remembered worktree, branch, resume', () => {
    expect(initialForm(incoming(), suggested)).toEqual({
      mode: 'existing',
      repo: '/home/u/code/samplehub',
      folder: '~/',
      cloneTo: '/home/u/code/samplehub-handoff',
      worktree: '/home/u/code/samplehub-handoff-feat-x',
      branch: 'feat/x',
      resume: true,
      trustMise: false,
      trustDirenv: false,
    });
    const none = initialForm(incoming(), { repos: [], repo: null, worktree_path: null, branch: 'feat/x' });
    expect(none).toMatchObject({ mode: 'clone', repo: '', cloneTo: '~/samplehub', worktree: '' });
  });

  test('builds accept params for each repository choice', () => {
    expect(acceptParams('in1', form())).toEqual({
      ok: true,
      params: { id: 'in1', repo: { path: '/home/u/code/samplehub' }, worktree_path: '/home/u/code/samplehub-handoff-feat-x', branch: 'feat/x', start_agent: true },
    });
    expect(acceptParams('in1', form({ mode: 'folder', folder: '~/kode/samplehub/', worktree: '', resume: false, trustMise: true, trustDirenv: true }))).toEqual({
      ok: true,
      params: { id: 'in1', repo: { path: '~/kode/samplehub' }, branch: 'feat/x', start_agent: false, trust: ['mise', 'direnv'] },
    });
    const clone = acceptParams('in1', form({ mode: 'clone', cloneTo: '~/src/samplehub' }));
    expect(clone.ok && clone.params.repo).toEqual({ clone_to: '~/src/samplehub' });
  });

  test('reports the field that needs fixing', () => {
    expect(acceptParams('in1', form({ mode: 'folder', folder: '~/' }))).toEqual({ ok: false, problem: 'folder' });
    expect(acceptParams('in1', form({ mode: 'clone', cloneTo: 'relative/path' }))).toEqual({ ok: false, problem: 'clone' });
    expect(acceptParams('in1', form({ repo: '' }))).toEqual({ ok: false, problem: 'repo' });
    expect(acceptParams('in1', form({ worktree: 'here' }))).toEqual({ ok: false, problem: 'worktree' });
    expect(acceptParams('in1', form({ branch: 'has space' }))).toEqual({ ok: false, problem: 'branch' });
  });

  test('branch names and paths', () => {
    for (const b of ['feat/x', 'fix-1', 'a.b', 'user/ticket_42']) expect(validBranch(b)).toBe(true);
    for (const b of ['', '-x', '/x', 'x/', 'a..b', 'a b', 'x.lock', 'a~1', 'a^', 'a:b', 'a?', 'a*', 'a[', 'a\\b', '@', 'a@{1}']) expect(validBranch(b)).toBe(false);
    expect(cleanPath(' ~/code/x/ ')).toBe('~/code/x');
    expect(cleanPath('~/')).toBe('~');
    expect(cleanPath('/')).toBe('/');
  });

  test('the radio list keeps a folder chosen by hand', () => {
    expect(repoChoices(suggested, '/home/u/code/samplehub')).toEqual(['/home/u/code/samplehub']);
    expect(repoChoices({ ...suggested, repos: [] }, '/x')).toEqual(['/home/u/code/samplehub', '/x']);
  });

  test('repo_mismatch details from an RPC error or a failed record', () => {
    const details = { reason: 'repo_mismatch', repo: '/r', origin: 'git@github.com:a/b.git', remotes: ['git@github.com:c/d.git', 7] };
    expect(repoMismatch({ data: { kind: 'conflict', details } })).toEqual({ repo: '/r', origin: 'git@github.com:a/b.git', remotes: ['git@github.com:c/d.git'] });
    expect(repoMismatch({ kind: 'conflict', message: 'x', details })).toMatchObject({ repo: '/r' });
    expect(repoMismatch({ kind: 'conflict', message: 'x' })).toBeNull();
    expect(repoMismatch(null)).toBeNull();
  });
});

describe('incoming list', () => {
  test('events upsert, keep the newest copy, and remove expired records', () => {
    const rec = incoming();
    const c = incomingChange(event('handoff.incoming', { incoming: rec }, { incoming: 'in1' }));
    expect(c).toEqual({ k: 'upsert', record: rec, phase: null });
    let list = applyIncoming([], c as Exclude<typeof c, null | { k: 'refetch' }>);
    expect(list.map((r) => r.id)).toEqual(['in1']);
    const upd = incomingChange(event('handoff.updated', { incoming: { ...rec, state: 'importing', updated_at_ms: 2000 }, phase: 'cloning' }));
    expect(upd).toMatchObject({ k: 'upsert', phase: 'cloning' });
    list = applyIncoming(list, upd as Exclude<typeof upd, null | { k: 'refetch' }>);
    expect(list[0]!.state).toBe('importing');
    // An older copy arriving late does not win.
    list = applyIncoming(list, { k: 'upsert', record: rec, phase: null });
    expect(list[0]!.state).toBe('importing');
    expect(incomingChange(event('handoff.expired', {}, { incoming: 'in1' }))).toEqual({ k: 'remove', id: 'in1' });
    expect(applyIncoming(list, { k: 'remove', id: 'in1' })).toEqual([]);
    expect(incomingChange(event('handoff.updated', { incoming: 'in1' }))).toEqual({ k: 'refetch' });
    expect(incomingChange(event('agent.started', {}))).toBeNull();
  });

  test('waiting ones first and counted for the badge', () => {
    const list = [
      incoming({ id: 'done', state: 'imported', created_at_ms: 5 }),
      incoming({ id: 'p-old', state: 'pending', created_at_ms: 1 }),
      incoming({ id: 'f', state: 'failed', created_at_ms: 3 }),
      incoming({ id: 'imp', state: 'importing', created_at_ms: 4 }),
    ];
    expect(sortIncoming(list).map((r) => r.id)).toEqual(['f', 'p-old', 'imp', 'done']);
    expect(actionCount([list, [incoming({ state: 'declined' })]])).toBe(2);
  });

  test('an imported handoff opens at its pane, else its workspace', () => {
    expect(importedTarget(incoming({ state: 'imported', result: { pane: 'p9', workspace: 'w1' } }))).toEqual({ pane: 'p9' });
    expect(importedTarget(incoming({ state: 'imported', result: { workspace: 'w1', pane: null } }))).toEqual({ workspace: 'w1' });
    expect(importedTarget(incoming())).toBeNull();
  });

  test('routes', () => {
    expect(parseRoute('#/handoffs')).toEqual({ name: 'handoffs', host: null, id: null });
    expect(parseRoute('#/handoffs/h1')).toEqual({ name: 'handoffs', host: 'h1', id: null });
    expect(parseRoute('#/handoffs/h1/in%2F1')).toEqual({ name: 'handoffs', host: 'h1', id: 'in/1' });
    expect(formatRoute({ name: 'handoffs', host: 'h1', id: 'in/1' })).toBe('#/handoffs/h1/in%2F1');
    expect(formatRoute({ name: 'handoffs', host: null, id: null })).toBe('#/handoffs');
  });
});
