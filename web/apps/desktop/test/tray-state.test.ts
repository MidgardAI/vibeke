import { describe, expect, test } from 'bun:test';
import type { AgentRun, Dashboard, Execution, HostState, Interaction } from '@vibeke/core';
import { TrayTracker } from '../src/main/tray-state';

const run = (id: string, value: Execution, done_rev = 0, o: Partial<AgentRun> = {}): AgentRun =>
  ({ id, pane: `p-${id}`, execution: { value, since_ms: 1, source: 'structured', confidence: 1, detail: null }, done_rev, ended_at_ms: null, ...o }) as AgentRun;

const open = (id: string, runId: string): Interaction => ({ id, run: runId, pane: `p-${runId}`, kind: 'approval', status: 'open' }) as Interaction;

const host = (id: string, runs: AgentRun[], interactions: Interaction[] = [], dashboard = true): HostState =>
  ({
    record: { host_id: id, relay: 'wss://relay.example', hk: 'k', device_id: 'd', name: id, scope: 'full' },
    status: 'online', error: null, closeCode: null, info: null, cursor: 1, lastOnlineAt: 1, nextRetryAt: null,
    dashboard: dashboard ? ({ runs, interactions } as unknown as Dashboard) : null,
  }) as HostState;

describe('menu-bar summary', () => {
  test('counts open interactions, stopped agents and working agents across hosts', () => {
    const t = new TrayTracker();
    const s = t.summary([
      host('local', [run('a', 'working'), run('b', 'idle'), run('c', 'error')], [open('i1', 'b')]),
      host('remote', [run('d', 'rate_limited'), run('e', 'starting'), run('f', 'working', 0, { ended_at_ms: 5 })]),
    ]);
    expect(s).toEqual({ needs: 3, working: 2 });
  });

  test('an agent with an open interaction counts once', () => {
    const t = new TrayTracker();
    expect(t.summary([host('h', [run('a', 'error')], [open('i1', 'a'), open('i2', 'a')])])).toEqual({ needs: 2, working: 0 });
  });

  test('a finish counts until the popover is opened; finishes before startup do not', () => {
    const t = new TrayTracker();
    expect(t.summary([host('h', [run('a', 'idle', 3)])]).needs).toBe(0);
    expect(t.summary([host('h', [run('a', 'working', 3)])])).toEqual({ needs: 0, working: 1 });
    const done = [host('h', [run('a', 'idle', 4)])];
    expect(t.summary(done).needs).toBe(1);
    t.markSeen(done);
    expect(t.summary(done).needs).toBe(0);
    // The next finish counts again.
    expect(t.summary([host('h', [run('a', 'idle', 5)])]).needs).toBe(1);
  });

  test('a host without a dashboard (reconnecting) keeps what was seen', () => {
    const t = new TrayTracker();
    t.summary([host('h', [run('a', 'working', 1)])]);
    expect(t.summary([host('h', [], [], false)])).toEqual({ needs: 0, working: 0 });
    expect(t.summary([host('h', [run('a', 'idle', 2)])]).needs).toBe(1);
  });

  test('a removed host is forgotten', () => {
    const t = new TrayTracker();
    t.summary([host('h', [run('a', 'working', 1)])]);
    t.summary([]);
    // Seen again from scratch: its current finish is old news.
    expect(t.summary([host('h', [run('a', 'idle', 2)])]).needs).toBe(0);
  });
});
