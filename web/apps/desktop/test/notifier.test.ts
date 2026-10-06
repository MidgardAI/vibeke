// Notifier lifecycle with fakes (no Electron): preferences fail closed and are shared per host,
// interactions answered while preferences load are not announced, finished notifications are
// tracked by host (not tag), and resolutions update the merged content.

import { describe, expect, test } from 'bun:test';
import { RpcError, type AgentRun, type Dashboard, type HostState, type Interaction } from '@vibeke/core';
import { Notifier, type NotificationLike, type NotifierEngine } from '../src/main/notifier';

const interaction = (id: string, command = 'pnpm test'): Interaction =>
  ({ id, run: 'r1', pane: 'p1', kind: 'approval', status: 'open', title: 't', action: { tool: 'Bash', command, risk: 'low' } }) as unknown as Interaction;
const run = (o: Partial<AgentRun> = {}): AgentRun => ({ id: 'r1', pane: 'p1', harness: 'codex', execution: { value: 'working' }, done_rev: 0, ...o }) as unknown as AgentRun;
const dash = (interactions: Interaction[], runs: AgentRun[] = [run()]): Dashboard =>
  ({ at: 1, session: 's', machine: 'm', workspaces: [], tabs: [], panes: [], runs, interactions, tasks: [], notifications_unread: 0 }) as unknown as Dashboard;
const host = (d: Dashboard, id = 'h1'): HostState => ({
  record: { host_id: id, relay: 'local:/x', hk: 'k', device_id: 'd', name: 'secret-host', scope: 'full' },
  status: 'online', error: null, closeCode: null, info: null, dashboard: d, cursor: 1, lastOnlineAt: 1, nextRetryAt: null,
});

const flush = async (n = 10) => {
  for (let i = 0; i < n; i++) await Promise.resolve();
};

function setup() {
  let states: HostState[] = [];
  const subs = new Set<() => void>();
  const requests: string[] = [];
  const replies: { resolve(v: unknown): void; reject(e: unknown): void }[] = [];
  const engine: NotifierEngine = {
    manager: { getSnapshot: () => states, subscribe: (cb) => (subs.add(cb), () => subs.delete(cb)) },
    request: (h, m) => {
      requests.push(`${h}:${m}`);
      return new Promise((resolve, reject) => replies.push({ resolve, reject }));
    },
  };
  const shown: { title: string; body: string; silent: boolean; closed: boolean }[] = [];
  const create = (o: { title?: string; body?: string; silent?: boolean }): NotificationLike => {
    const rec = { title: o.title ?? '', body: o.body ?? '', silent: !!o.silent, closed: false };
    const n: NotificationLike = { show: () => void shown.push(rec), close: () => void (rec.closed = true), on: () => n };
    return n;
  };
  const notifier = new Notifier(engine, { appFocused: () => false, enabled: () => true, openMain: () => {}, approve: () => {} }, create as never);
  const set = (s: HostState[]) => {
    states = s;
    for (const cb of [...subs]) cb();
  };
  return { notifier, set, requests, replies, shown };
}

const prefs = (privacy: string, o: Record<string, unknown> = {}, dnd = 0) => ({ device: { privacy, notify_input: true, notify_done: true, ...o }, host: { dnd_until: dnd } });

describe('notifier', () => {
  test('nothing is shown before preferences are known; one prefs.get per host', async () => {
    const { notifier, set, requests, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start(); // prefetch starts
    set([host(dash([interaction('i1')]))]);
    set([host(dash([interaction('i1'), interaction('i2')]))]);
    await flush();
    expect(requests).toEqual(['h1:prefs.get']); // concurrent callers share it
    expect(shown).toHaveLength(0);
    // Minimal privacy, DND on: still nothing, and the host name never leaked meanwhile.
    replies[0]!.resolve(prefs('minimal', {}, Date.now() / 1000 + 600));
    await flush();
    expect(shown).toHaveLength(0);
  });

  test('a failed fetch with nothing cached shows nothing; cached restrictive prefs are kept', async () => {
    const { notifier, set, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start();
    replies[0]!.reject(new Error('timeout'));
    await flush();
    set([host(dash([interaction('i1')]))]);
    await flush();
    replies[1]!.reject(new Error('offline'));
    await flush();
    expect(shown).toHaveLength(0);
    // Now known (minimal) …
    set([host(dash([]))]);
    set([host(dash([interaction('i2')]))]);
    await flush();
    replies[2]!.resolve(prefs('minimal'));
    await flush();
    expect(shown.at(-1)).toMatchObject({ title: 'Vibeke', body: '1 agent needs you' });
    // … and after an invalidation whose refetch fails, still minimal (never the defaults).
    notifier.invalidatePrefs('h1');
    replies[3]!.reject(new Error('offline'));
    set([host(dash([interaction('i2'), interaction('i3')]))]);
    await flush();
    expect(shown.at(-1)).toMatchObject({ title: 'Vibeke', body: '2 agents need you' });
    expect(shown.every((s) => !s.title.includes('secret-host') && !s.body.includes('secret-host'))).toBe(true);
  });

  test('an interaction answered while preferences load is not announced', async () => {
    const { notifier, set, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start();
    set([host(dash([interaction('i1')]))]);
    set([host(dash([]))]); // answered elsewhere
    await flush();
    replies[0]!.resolve(prefs('full'));
    await flush();
    expect(shown).toHaveLength(0);
  });

  test('older gateways without prefs.get get the most private behaviour', async () => {
    const { notifier, set, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start();
    replies[0]!.reject(new RpcError('prefs.get', { code: -32601, message: 'method not found' }));
    await flush();
    set([host(dash([interaction('i1')]))]);
    await flush();
    expect(shown.at(-1)).toMatchObject({ title: 'Vibeke', body: '1 agent needs you' });
  });

  test('resolutions update the merged content quietly', async () => {
    const { notifier, set, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start();
    replies[0]!.resolve(prefs('summary'));
    await flush();
    set([host(dash([interaction('i1'), interaction('i2')]))]);
    await flush();
    expect(shown.at(-1)).toMatchObject({ title: '2 agents need you', silent: false });
    set([host(dash([interaction('i2')]))]);
    await flush();
    expect(shown.at(-2)!.closed).toBe(true);
    expect(shown.at(-1)).toMatchObject({ title: 'Codex needs approval', silent: true });
    set([host(dash([]))]);
    await flush();
    expect(shown.at(-1)!.closed).toBe(true);
  });

  test('a finished notification is tracked by its host, not its tag', async () => {
    const orig = globalThis.setTimeout;
    const timers: (() => void)[] = [];
    globalThis.setTimeout = ((f: () => void) => (timers.push(f), 0)) as never;
    try {
      const { notifier, set, replies, shown } = setup();
      const idle = (rev: number) => run({ execution: { value: 'idle' } as never, done_rev: rev });
      set([host(dash([], [run()]))]);
      notifier.start();
      replies[0]!.resolve(prefs('summary'));
      await flush();
      set([host(dash([], [idle(1)]))]);
      timers.shift()!();
      await flush();
      expect(shown.at(-1)).toMatchObject({ title: 'Codex finished' });
      // The next update for the same (still present) host must not close it.
      set([host(dash([], [idle(1)]))]);
      set([{ ...host(dash([], [idle(1)])), cursor: 2 }]);
      await flush();
      expect(shown.at(-1)!.closed).toBe(false);
      // Forgetting the host does.
      set([]);
      expect(shown.at(-1)!.closed).toBe(true);
    } finally {
      globalThis.setTimeout = orig;
    }
  });
});
