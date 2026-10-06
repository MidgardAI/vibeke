import { describe, expect, test } from 'bun:test';
import { alertPayload, stripCommand, type Dashboard, type HostState, type Interaction, type AgentRun } from '@vibeke/core';
import { AlertTracker, alertAllowed, alertPrefsFrom, payloadFor, DEFAULT_ALERT_PREFS } from '../src/main/alerts';

const run = (o: Partial<AgentRun> = {}): AgentRun =>
  ({
    id: 'r1', handle: 'a1', name: null, pane: 'p1', harness: 'codex', harness_version: null, integration: 'hooks', harness_session_id: null,
    transcript_path: null, resume_argv: [], cwd: '/src/samplehub', model: null, task: null,
    execution: { value: 'working', since_ms: 1, source: 'structured', confidence: 1, detail: null },
    health: 'healthy', yolo: false, permission_mode: null, last_message: null, last_tool: null, turns_completed: 0, done_rev: 0,
    started_at_ms: 0, ended_at_ms: null, capabilities: [], ...o,
  }) as AgentRun;

const interaction = (o: Partial<Interaction> = {}): Interaction =>
  ({
    id: 'i1', handle: 'i1', run: 'r1', pane: 'p1', kind: 'approval', status: 'open', title: 'Bash: pnpm test', body_md: null,
    action: { tool: 'Bash', summary: 'Bash', command: 'pnpm test --token=sk-ant-abcdefghijk', paths: [], diff: null, risk: 'low', risk_reasons: [] },
    questions: [], plan_md: null, answer_channel: 'native', native_ref: null, source: 'structured', confidence: 1, answerable: true, gate: true,
    decision_rev: 1, delivery: 'none', delivery_error: null, answer: null, answered_by: null, opened_at_ms: 1, answered_at_ms: null, ...o,
  }) as Interaction;

const dash = (interactions: Interaction[], runs: AgentRun[] = [run()]): Dashboard => ({
  at: 1, session: 'default', machine: 'devbox',
  workspaces: [{ id: 'w1', handle: 'w1', name: 'samplehub', auto_name: 'samplehub', root_path: '/src/samplehub', task: null, order: 1, branch: null }],
  tabs: [], panes: [{ id: 'p1', workspace: 'w1' } as never], runs, interactions, tasks: [], notifications_unread: 0,
});

const host = (d: Dashboard | null, id = 'h1'): HostState => ({
  record: { host_id: id, relay: 'local:/x.sock', hk: 'k', device_id: 'd', name: 'devbox', scope: 'full' },
  status: 'online', error: null, closeCode: null, info: null, dashboard: d, cursor: 1, lastOnlineAt: 1, nextRetryAt: null,
});

describe('notification content (§7.8 privacy levels)', () => {
  const item = { key: 'i1', title: 'Codex · samplehub wants to run `pnpm test --token=sk-ant-abcdefghijk`', url: '#/i/h1/i1', urgent: true };
  test('full redacts secrets but keeps the command', () => {
    const p = alertPayload([item], 'full', 'devbox')!;
    expect(p.title).toBe('Codex · samplehub wants to run `pnpm test --token=[REDACTED]`');
    expect(p.body).toBe('devbox');
    expect(p.url).toBe('#/i/h1/i1');
  });
  test('summary drops the command', () => {
    expect(alertPayload([item], 'summary', 'devbox')!.title).toBe('Codex · samplehub needs approval');
    expect(stripCommand('Claude · x has a question')).toBe('Claude · x has a question');
  });
  test('minimal says nothing specific', () => {
    expect(alertPayload([item], 'minimal', 'devbox')).toEqual({ title: 'Vibeke', body: '1 agent needs you', url: '#/i/h1/i1', count: 1 });
  });
  test('several items merge into one per host and open the inbox', () => {
    const p = alertPayload([item, { ...item, key: 'i2' }, { ...item, key: 'i3' }], 'full', 'devbox')!;
    expect(p).toEqual({ title: '3 agents need you', body: 'devbox', url: '#/inbox', count: 3 });
    expect(alertPayload([], 'full', 'x')).toBeNull();
  });
  test('prefs parsing and DND', () => {
    const p = alertPrefsFrom({ device: { privacy: 'minimal', notify_input: false, notify_done: true }, host: { dnd_until: 2000 } });
    expect(p).toEqual({ privacy: 'minimal', notify_input: false, notify_done: true, dnd_until: 2000 });
    expect(alertPrefsFrom(null)).toEqual(DEFAULT_ALERT_PREFS);
    expect(alertPrefsFrom({ device: { privacy: 'everything' } }).privacy).toBe('summary');
    expect(alertAllowed({ ...DEFAULT_ALERT_PREFS, dnd_until: 2000 }, 1_999_000, 'input')).toBe(false);
    expect(alertAllowed({ ...DEFAULT_ALERT_PREFS, dnd_until: 2000 }, 2_001_000, 'input')).toBe(true);
    expect(alertAllowed(DEFAULT_ALERT_PREFS, 0, 'done')).toBe(false);
  });
});

describe('alert tracker', () => {
  test('baseline first: interactions already open at startup do not notify', () => {
    const t = new AlertTracker();
    expect(t.update([host(dash([interaction()]))]).changes).toEqual([]);
  });

  test('new interaction → one change with a described title; resolution empties it', () => {
    const t = new AlertTracker();
    t.update([host(dash([]))]);
    const { changes } = t.update([host(dash([interaction()]))]);
    expect(changes).toHaveLength(1);
    expect(changes[0]!.added).toBe(true);
    expect(changes[0]!.approvable).toBe('i1');
    expect(changes[0]!.items[0]!.title).toBe('Codex · samplehub wants to run `pnpm test --token=[REDACTED]`'); // redacted before any truncation
    const p = payloadFor(changes[0]!, DEFAULT_ALERT_PREFS)!;
    expect(p.title).toBe('Codex · samplehub needs approval'); // default privacy is summary
    const gone = t.update([host(dash([]))]).changes;
    expect(gone).toEqual([{ hostId: 'h1', hostName: 'devbox', items: [], added: false, approvable: null }]);
  });

  test('notices are not alerts; two open → not approvable from a notification', () => {
    const t = new AlertTracker();
    t.update([host(dash([]))]);
    expect(t.update([host(dash([interaction({ kind: 'notice' })]))]).changes).toEqual([]);
    const c = t.update([host(dash([interaction({ kind: 'notice' }), interaction({ id: 'i2' }), interaction({ id: 'i3', kind: 'question', action: null })]))]).changes[0]!;
    expect(c.items.map((i) => i.key)).toEqual(['i2', 'i3']);
    expect(c.items[1]!.title).toBe('Codex · samplehub has a question');
    expect(c.approvable).toBeNull();
  });

  test('stopped agents alert; finished agents become done candidates', () => {
    const t = new AlertTracker();
    t.update([host(dash([], [run()]))]);
    const err = t.update([host(dash([], [run({ execution: { value: 'rate_limited', since_ms: 2, source: 'structured', confidence: 1, detail: null } })]))]).changes[0]!;
    expect(err.items[0]).toMatchObject({ key: 'run:r1', title: 'Codex · samplehub is rate limited', urgent: false });
    const working = run({ execution: { value: 'working', since_ms: 3, source: 'structured', confidence: 1, detail: null } });
    expect(t.update([host(dash([], [working]))]).changes[0]!.items).toEqual([]);
    const idle = run({ done_rev: 1, execution: { value: 'idle', since_ms: 4, source: 'structured', confidence: 1, detail: null } });
    const states = [host(dash([], [idle]))];
    const { done } = t.update(states);
    expect(done).toEqual([{ hostId: 'h1', runId: 'r1', doneRev: 1 }]);
    expect(t.confirmDone(states, done[0]!)).toMatchObject({ title: 'Codex · samplehub finished', url: '#/r/h1/r1' });
    // Working again (or an open interaction) cancels "finished".
    expect(t.confirmDone([host(dash([], [working]))], done[0]!)).toBeNull();
    expect(t.confirmDone([host(dash([interaction()], [idle]))], done[0]!)).toBeNull();
  });

  test('hosts without a dashboard are skipped; forgotten hosts are dropped', () => {
    const t = new AlertTracker();
    expect(t.update([host(null)]).changes).toEqual([]);
    t.update([host(dash([]))]);
    t.update([]);
    // Back again: it is a new baseline (no alert for what is already open).
    expect(t.update([host(dash([interaction()]))]).changes).toEqual([]);
  });
});
