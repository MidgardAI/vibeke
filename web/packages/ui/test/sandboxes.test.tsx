import { describe, expect, test } from 'bun:test';
import { renderToStaticMarkup } from 'react-dom/server';
import { RpcError, type CloudBox, type CloudProvider } from '@vibeke/core';
import { SandboxGroups } from '../src/screens/sandboxes';
import { boxActions, boxCounts, cloudBoxFromEvent, cloudJobFromEvent, groupBoxes, parseCloudBox, tryDestroy, upsertCloudBox } from '../src/lib/cloud';

const provider = (id: string, state: 'ok' | 'missing' = 'ok'): CloudProvider => ({ id, label: id === 'sprites' ? 'Sprites' : 'E2B', caps: {}, default: id === 'sprites', auth: { state, account: state === 'ok' ? 'kari' : undefined }, methods: [] });

const box = (id: string, p: Partial<CloudBox> = {}): CloudBox => ({
  box: `sprites/${id}`,
  provider: 'sprites',
  id,
  name: `vk-${id}`,
  state: 'running',
  ownership: 'attached',
  task: `task ${id}`,
  panes: ['p1'],
  unsynced: null,
  caps: { checkpoints: true, explicit_suspend: true },
  created_at: 1000,
  last_activity_at: 2000,
  ...p,
});

describe('SandboxGroups', () => {
  const groups = groupBoxes([provider('sprites'), provider('e2b', 'missing')], [box('a'), box('b', { state: 'paused', ownership: 'idle' }), box('c', { unsynced: { commits: 2, dirty: 1, untracked: 0, summary: '2 commits not on host' } })]);
  const html = renderToStaticMarkup(<SandboxGroups groups={groups} now={5_000_000} busy={null} onAction={() => {}} onSignIn={() => {}} onSignOut={() => {}} />);

  test('groups boxes under their provider with the sign-in state', () => {
    expect(groups.map((g) => [g.provider, g.boxes.length])).toEqual([
      ['sprites', 3],
      ['e2b', 0],
    ]);
    expect(html).toContain('Signed in as kari');
    expect(html).toContain('Not signed in');
    expect(html).toContain('Sign in');
  });

  test('shows the box name (not the task id), state, ownership and the unsynced marker', () => {
    expect(html).toContain('vk-a');
    expect(html).not.toContain('task a');
    expect(html).toContain('paused');
    expect(html).toContain('Idle');
    expect(html).toContain('Not synced');
    expect(html).toContain('2 commits not on host');
  });

  test('offers row actions by capability and state', () => {
    expect(boxActions(box('a'))).toEqual(['open', 'bring_back', 'suspend', 'checkpoint', 'destroy']);
    expect(boxActions(box('b', { state: 'paused', ownership: 'idle' }))).toContain('resume');
    expect(boxActions(box('o', { ownership: 'orphaned', panes: [] }))).toEqual(['adopt', 'suspend', 'checkpoint', 'destroy']);
    expect(boxActions(box('m', { ownership: 'missing' }))).toEqual(['forget']);
    expect(boxActions(box('n', { caps: { explicit_suspend: false } }))).not.toContain('suspend');
  });

  test('counts running and idle boxes', () => {
    expect(boxCounts(groups.flatMap((g) => g.boxes))).toEqual({ running: 2, idle: 1 });
  });
});

describe('tryDestroy', () => {
  const conflict = new RpcError('cloud.box.destroy', { code: -32000, message: 'unsynced', data: { kind: 'conflict', details: { reason: 'unsynced_changes', unsynced: { commits: 1, dirty: 0, untracked: 0, summary: '1 commit' } } } });

  const fake = () => {
    const calls: { box: string; force?: boolean }[] = [];
    return {
      calls,
      request: async (_m: 'cloud.box.destroy', p: { box: string; force?: boolean }) => {
        calls.push(p);
        if (!p.force) throw conflict;
        return { box: p.box, destroyed: true as const };
      },
    };
  };

  test('reports unsynced work instead of failing', async () => {
    const c = fake();
    const r = await tryDestroy(c, 'sprites/a');
    expect(r.k).toBe('unsynced');
    expect(r.k === 'unsynced' && r.unsynced?.summary).toBe('1 commit');
    expect(c.calls).toEqual([{ box: 'sprites/a' }]);
  });

  test('"Destroy anyway" sends force', async () => {
    const c = fake();
    expect((await tryDestroy(c, 'sprites/a', true)).k).toBe('destroyed');
    expect(c.calls).toEqual([{ box: 'sprites/a', force: true }]);
  });

  test('other errors reach the caller', async () => {
    const c = { request: async () => { throw new Error('offline'); } };
    await expect(tryDestroy(c, 'sprites/a')).rejects.toThrow('offline');
  });
});

describe('events', () => {
  const ev = (type: string, subject: Record<string, string>, data: Record<string, unknown>) => ({ seq: 1, ts: 1, type, subject, data });

  test('cloud.job carries the job as data', () => {
    const j = cloudJobFromEvent(ev('cloud.job', { job: 'j1' }, { state: 'uploading', direction: 'send', progress: { done: 1, total: 4 }, updated_at: 3 }));
    expect(j?.id).toBe('j1');
    expect(j?.state).toBe('uploading');
    expect(j?.progress).toEqual({ done: 1, total: 4 });
  });

  test('cloud.box.changed updates the list and a destroyed box leaves it', () => {
    const b = cloudBoxFromEvent(ev('cloud.box.changed', { box: 'sprites/a' }, { state: 'paused', ownership: 'idle', panes: [] }));
    expect(b?.box).toBe('sprites/a');
    expect(b?.state).toBe('paused');
    const list = upsertCloudBox([box('a')], b!);
    expect(list.map((x) => x.state)).toEqual(['paused']);
    expect(upsertCloudBox(list, parseCloudBox({ box: 'sprites/a', state: 'destroyed' })!)).toEqual([]);
  });
});
