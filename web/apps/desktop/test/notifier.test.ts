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

/** Replace setTimeout for one test: returns the queued callbacks (run them by hand). */
function fakeTimers() {
  const orig = globalThis.setTimeout;
  const origClear = globalThis.clearTimeout;
  const timers: { f: () => void; ms: number; cleared: boolean }[] = [];
  globalThis.setTimeout = ((f: () => void, ms: number) => {
    const t = { f, ms, cleared: false };
    timers.push(t);
    return t;
  }) as never;
  globalThis.clearTimeout = ((t: { cleared: boolean } | undefined) => {
    if (t && typeof t === 'object') t.cleared = true;
  }) as never;
  const live = () => timers.filter((t) => !t.cleared);
  const fire = () => {
    const t = live()[0];
    if (!t) throw new Error('no timer');
    t.cleared = true;
    t.f();
  };
  const restore = () => {
    globalThis.setTimeout = orig;
    globalThis.clearTimeout = origClear;
  };
  return { live, fire, restore };
}

function setup(hooks: { enabled?: () => boolean; appFocused?: () => boolean } = {}) {
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
  const notifier = new Notifier(engine, { appFocused: hooks.appFocused ?? (() => false), enabled: hooks.enabled ?? (() => true), openMain: () => {}, approve: () => {} }, create as never);
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
    // … and after an invalidation whose refetch fails, the new preferences are unknown: nothing
    // is delivered with the old (or default) ones; the shown notification is withdrawn.
    const before = shown.length;
    notifier.invalidatePrefs('h1');
    replies[3]!.reject(new Error('offline'));
    set([host(dash([interaction('i2'), interaction('i3')]))]);
    await flush();
    expect(shown).toHaveLength(before);
    expect(shown.at(-1)!.closed).toBe(true);
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

  test('an obsolete preferences response is never used to deliver (P1)', async () => {
    const { notifier, set, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start(); // gen 0 request: replies[0]
    set([host(dash([interaction('i1', 'curl secret.example/token')]))]);
    await flush();
    notifier.invalidatePrefs('h1'); // gen 1 request: replies[1]
    // The obsolete (permissive) response arrives first: waiters must not deliver with it.
    replies[0]!.resolve(prefs('full'));
    await flush();
    expect(shown).toHaveLength(0);
    // The current generation says minimal + DND: still nothing.
    replies[1]!.resolve(prefs('minimal', {}, Date.now() / 1000 + 600));
    await flush();
    expect(shown).toHaveLength(0);
  });

  test('waiters on an obsolete response deliver with the current generation\'s preferences', async () => {
    const { notifier, set, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start();
    set([host(dash([interaction('i1', 'curl secret.example/token')]))]);
    await flush();
    notifier.invalidatePrefs('h1');
    replies[0]!.resolve(prefs('full'));
    await flush();
    replies[1]!.resolve(prefs('minimal'));
    await flush();
    expect(shown).toHaveLength(1);
    expect(shown[0]).toMatchObject({ title: 'Vibeke', body: '1 agent needs you' });
  });

  for (const [name, o] of [
    ['notifications disabled', { enabled: false }],
    ['window focused', { focused: true }],
    ['DND on', { dnd: true }],
    ['notify_input off', { notifyInput: false }],
  ] as const) {
    test(`a resolution update is gated like a new alert: ${name}`, async () => {
      let enabled = true;
      let focused = false;
      const { notifier, set, replies, shown } = setup({ enabled: () => enabled, appFocused: () => focused });
      set([host(dash([]))]);
      notifier.start();
      replies[0]!.resolve(prefs('full'));
      await flush();
      set([host(dash([interaction('i1'), interaction('i2', 'cat ~/.ssh/id_rsa')]))]);
      await flush();
      expect(shown).toHaveLength(1);
      // Gate changes, then one item resolves.
      if ('enabled' in o) enabled = false;
      if ('focused' in o) focused = true;
      if ('dnd' in o || 'notifyInput' in o) {
        notifier.invalidatePrefs('h1');
        replies[1]!.resolve('dnd' in o ? prefs('full', {}, Date.now() / 1000 + 600) : prefs('full', { notify_input: false }));
        await flush();
      }
      set([host(dash([interaction('i2', 'cat ~/.ssh/id_rsa')]))]);
      await flush();
      expect(shown).toHaveLength(1); // not recreated
      expect(shown[0]!.closed).toBe(true); // withdrawn
    });
  }

  test('a transient preferences failure does not drop the alert: retried and replayed', async () => {
    const timers = fakeTimers();
    try {
      const { notifier, set, replies, requests, shown } = setup();
      set([host(dash([]))]);
      notifier.start();
      set([host(dash([interaction('i1')]))]);
      await flush();
      replies[0]!.reject(new Error('timeout'));
      await flush();
      expect(shown).toHaveLength(0);
      // A retry is scheduled (backoff) …
      expect(timers.live()).toHaveLength(1);
      const first = timers.live()[0]!.ms;
      timers.fire();
      await flush();
      expect(requests).toHaveLength(2);
      replies[1]!.reject(new Error('timeout'));
      await flush();
      expect(timers.live()).toHaveLength(1);
      expect(timers.live()[0]!.ms).toBeGreaterThan(first);
      timers.fire();
      await flush();
      // … and once preferences load, the still-open interaction is announced.
      replies[2]!.resolve(prefs('summary'));
      await flush();
      expect(shown).toHaveLength(1);
      expect(shown[0]).toMatchObject({ title: 'Codex needs approval', silent: false });
      expect(timers.live()).toHaveLength(0);
    } finally {
      timers.restore();
    }
  });

  test('an alert pending on preferences that loaded via another path is replayed', async () => {
    const timers = fakeTimers();
    try {
      const { notifier, set, replies, shown } = setup();
      set([host(dash([]))]);
      notifier.start();
      set([host(dash([interaction('i1')]))]);
      await flush();
      replies[0]!.reject(new Error('timeout'));
      await flush();
      notifier.invalidatePrefs('h1'); // e.g. the renderer saved prefs
      replies[1]!.resolve(prefs('summary'));
      await flush();
      expect(shown).toHaveLength(1);
      expect(timers.live()).toHaveLength(0); // the retry was cancelled
      notifier.stop();
    } finally {
      timers.restore();
    }
  });

  test('a pane approval request raises an alert that its end withdraws', async () => {
    const { notifier, set, replies, shown } = setup();
    set([host(dash([]))]);
    notifier.start();
    replies[0]!.resolve(prefs('summary'));
    await flush();
    const ev = (type: string) => ({ seq: 1, ts: 1, type, subject: { pane: 'p1', request: 'q1' }, data: { method: 'handoff.send', summary: 'Send work to peer b' } });
    notifier.onEvent('h1', ev('auth.approval_requested') as never);
    await flush();
    expect(shown.map((n) => [n.title, n.body])).toEqual([['A pane asks to send a handoff', 'Send work to peer b']]);
    notifier.onEvent('h1', ev('auth.approval_denied') as never);
    expect(shown[0]!.closed).toBe(true);
  });
});
