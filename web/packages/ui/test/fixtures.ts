import type { AgentRun, Dashboard, HostState, Interaction, Pane, Tab, Workspace } from '@vibeke/core';

export function interaction(p: Partial<Interaction> = {}): Interaction {
  return {
    id: 'i1', handle: 'i1', run: 'r1', pane: 'p1', kind: 'approval', status: 'open', title: 'Run tests', body_md: null,
    action: { tool: 'Bash', summary: 'run', command: 'pnpm test', paths: [], diff: null, risk: 'low', risk_reasons: [] },
    questions: [], plan_md: null, answer_channel: 'native', native_ref: null, source: 'structured', confidence: 1,
    answerable: true, gate: true, decision_rev: 1, delivery: 'none', delivery_error: null, answer: null, answered_by: null,
    opened_at_ms: 1000, answered_at_ms: null, harness: 'claude', repo_root: '/r', ...p,
  };
}

export function run(p: Partial<AgentRun> = {}): AgentRun {
  return {
    id: 'r1', handle: 'r1', name: null, pane: 'p1', harness: 'claude', harness_version: null, integration: 'hooks',
    harness_session_id: null, transcript_path: null, resume_argv: [], cwd: '/r', model: null, task: null,
    execution: { value: 'idle', since_ms: 0, source: 'structured', confidence: 1, detail: null }, health: 'healthy', yolo: false,
    permission_mode: null, last_message: null, last_tool: null, turns_completed: 0, done_rev: 0, started_at_ms: 0,
    ended_at_ms: null, capabilities: [], ...p,
  };
}

export function pane(p: Partial<Pane> = {}): Pane {
  return {
    id: 'p1', handle: 'p1', tab: 't1', workspace: 'w1', title: null, auto_title: 'zsh', cwd: '/r', cols: 80, rows: 24,
    child_pid: null, fg_cmdline: [], exited: false, exit_code: null, unread: false, marked_unread: false, pinned: false,
    created_by: 'test', recovered: null, ...p,
  };
}

export const ws = (p: Partial<Workspace> = {}): Workspace => ({ id: 'w1', handle: 'w1', name: 'app', auto_name: 'app', root_path: '/r', task: null, order: 0, branch: null, ...p });
export const tab = (p: Partial<Tab> = {}): Tab => ({ id: 't1', handle: 't1', workspace: 'w1', title: null, number: 1, layout: { Leaf: { pane: 'p1' } }, focused_pane: null, zoomed_pane: null, order: 0, ...p });

export function dashboard(p: Partial<Dashboard> = {}): Dashboard {
  return { at: 1, session: 'main', machine: 'm', workspaces: [ws()], tabs: [tab()], panes: [pane()], runs: [run()], interactions: [], tasks: [], notifications_unread: 0, ...p };
}

export function host(id: string, d: Dashboard | null, status: HostState['status'] = 'online'): HostState {
  return {
    record: { host_id: id, relay: 'wss://r', hk: 'x', device_id: 'd', name: id, scope: 'full' },
    status, error: null, closeCode: null,
    info: { host_name: id, device_id: 'd', scope: 'full', server_version: '1', features: [] },
    dashboard: d, cursor: null, lastOnlineAt: null, nextRetryAt: null,
  };
}
