import { describe, expect, test } from 'bun:test';
import type { Execution, Task } from '@vibeke/core';
import { effectivePanel, layoutModeFor, resolveLegacy, togglePanelRoute } from '../src/app/selection';
import { buildTree } from '../src/lib/tree';
import { groupWorkspaces, mostUrgent, primaryPane, workspaceOfPane, workspacePinKey, workspaceRows } from '../src/lib/workspaces';
import { formatRoute, parseRoute, workspaceRoute, type Route } from '../src/router';
import { dashboard, host, interaction, pane, run, tab, ws } from './fixtures';

const exec = (value: Execution, since_ms = 0) => ({ value, since_ms, source: 'structured' as const, confidence: 1, detail: null });
const task = (p: Partial<Task>): Task => ({ id: 'k1', handle: 'k1', title: 'Task', slug: 'task', workspace: null, repo_root: '/r', worktree_path: null, branch: null, status: 'active', created_at_ms: 0, ...p });

/** One host, six workspaces (one per state), each with one pane `p<n>` in tab `t<n>`. */
function fixture() {
  const names = ['needs', 'review', 'working', 'done', 'idle', 'errored'];
  const d = dashboard({
    workspaces: names.map((n, i) => ws({ id: `w-${n}`, name: n, order: i, branch: n === 'working' ? 'feat/x' : null })),
    tabs: names.map((n, i) => tab({ id: `t-${n}`, workspace: `w-${n}`, layout: { Leaf: { pane: `p-${n}` } }, order: i })),
    panes: names.map((n) => pane({ id: `p-${n}`, tab: `t-${n}`, workspace: `w-${n}` })),
    runs: [
      run({ id: 'r-needs', pane: 'p-needs', execution: exec('working', 100) }),
      run({ id: 'r-review', pane: 'p-review', execution: exec('idle', 200), turns_completed: 3 }),
      run({ id: 'r-working', pane: 'p-working', harness: 'codex', execution: exec('working', 300) }),
      run({ id: 'r-done', pane: 'p-done', execution: exec('idle', 400), turns_completed: 2, done_rev: 4 }),
      run({ id: 'r-errored', pane: 'p-errored', execution: exec('error', 50) }),
    ],
    interactions: [interaction({ id: 'i1', run: 'r-needs', pane: 'p-needs', opened_at_ms: 150 })],
    tasks: [task({ id: 'k-review', workspace: 'w-review', title: 'Review me', review_label: 'ready_for_review', branch: 'task/review' })],
  });
  return d;
}

const treeOf = (d = fixture(), seen: Record<string, number> = {}, pins: string[] = []) => buildTree([host('h1', d)], { pins: new Set(pins), seenDone: seen });

describe('workspace routes', () => {
  test('parse with tab, panel, file and commit', () => {
    expect(parseRoute('#/w/h1/w1')).toEqual(workspaceRoute('h1', 'w1'));
    expect(parseRoute('#/w/h1/w1/t/p%2F2?panel=changes&file=src%2Fa.ts&commit=abc')).toMatchObject({
      name: 'workspace',
      host: 'h1',
      workspace: 'w1',
      pane: 'p/2',
      panel: 'changes',
      file: 'src/a.ts',
      commit: 'abc',
    });
    expect(parseRoute('#/w/h1/w1?panel=files')).toMatchObject({ panel: 'files' });
    expect(parseRoute('#/w/h1/w1?panel=off')).toMatchObject({ panel: 'off' });
    expect(parseRoute('#/w/h1/w1?panel=bogus')).toMatchObject({ panel: null });
    expect(parseRoute('#/w/h1')).toEqual({ name: 'not_found', path: '/w/h1' });
    expect(parseRoute('#/w/h1/w1/t')).toEqual({ name: 'not_found', path: '/w/h1/w1/t' });
    expect(parseRoute('#/w/h1/w1/x/p')).toEqual({ name: 'not_found', path: '/w/h1/w1/x/p' });
  });
  test('format round-trips', () => {
    const routes: Route[] = [
      workspaceRoute('h', 'w'),
      workspaceRoute('h 1', 'w/2', { pane: 'p 3' }),
      workspaceRoute('h', 'w', { panel: 'off' }),
      workspaceRoute('h', 'w', { pane: 'p', panel: 'changes', file: 'a b/c.ts', commit: 'deadbeef' }),
      workspaceRoute('h', 'w', { panel: 'files', file: 'x?y&z' }),
    ];
    for (const r of routes) expect(parseRoute(formatRoute(r))).toEqual(r);
    expect(formatRoute(workspaceRoute('h', 'w', { pane: 'p', panel: 'changes' }))).toBe('#/w/h/w/t/p?panel=changes');
    expect(formatRoute(workspaceRoute('h', 'w'))).toBe('#/w/h/w');
  });
  test('old routes still parse (redirected by the app)', () => {
    expect(parseRoute('#/panes')).toEqual({ name: 'panes' });
    expect(parseRoute('#/focus')).toEqual({ name: 'focus' });
    expect(parseRoute('#/changes')).toEqual({ name: 'changes' });
    expect(parseRoute('#/h/h1/p/p1/changes')).toEqual({ name: 'pane', host: 'h1', pane: 'p1', view: 'changes' });
  });
});

describe('groupWorkspaces', () => {
  test('one row per workspace in status groups, most urgent first', () => {
    const list = groupWorkspaces(treeOf(fixture(), { 'h1/r-done': 1 }));
    expect(list.groups.map((g) => g.id)).toEqual(['needs', 'review', 'working', 'done', 'idle']);
    const ids = (g: string) => list.groups.find((x) => x.id === g)!.rows.map((r) => r.workspace.id);
    // Needs you: an open interaction, and a run in error.
    expect(ids('needs').sort()).toEqual(['w-errored', 'w-needs']);
    expect(ids('review')).toEqual(['w-review']);
    expect(ids('working')).toEqual(['w-working']);
    expect(ids('done')).toEqual(['w-done']);
    expect(ids('idle')).toEqual(['w-idle']);
    expect(list.all[0]!.group).toBe('needs');
    expect(mostUrgent(list)!.group).toBe('needs');
  });
  test('row fields: title from the task, branch, harness, check label, activity, open count', () => {
    const rows = workspaceRows(treeOf());
    const by = (id: string) => rows.find((r) => r.workspace.id === id)!;
    expect(by('w-review').title).toBe('Review me');
    expect(by('w-review').branch).toBe('task/review');
    expect(by('w-review').checkLabel).toBe('ready for review');
    expect(by('w-working').branch).toBe('feat/x');
    expect(by('w-working').harness).toBe('codex');
    expect(by('w-working').lastActivityMs).toBe(300);
    expect(by('w-needs').open).toBe(1);
    expect(by('w-needs').lastActivityMs).toBe(150);
    expect(by('w-idle').harness).toBeNull();
    expect(by('w-idle').title).toBe('idle');
    expect(by('w-done').key).toBe('h1/w-done');
  });
  test('unseen finished runs are unread, seen ones are not', () => {
    expect(workspaceRows(treeOf(fixture(), { 'h1/r-done': 1 })).find((r) => r.workspace.id === 'w-done')!.unread).toBe(true);
    expect(workspaceRows(treeOf(fixture(), { 'h1/r-done': 4 })).find((r) => r.workspace.id === 'w-done')!.unread).toBe(false);
  });
  test('attention hints (attention.list) mark needs-you and review', () => {
    const list = groupWorkspaces(treeOf(), {
      attention: [
        { host: 'h1', workspace: 'w-idle', kind: 'review' },
        { host: 'h1', pane: 'p-done', kind: 'needs_you' },
        { host: 'other', workspace: 'w-working', kind: 'needs_you' },
      ],
    });
    const group = (id: string) => list.all.find((r) => r.workspace.id === id)!.group;
    expect(group('w-idle')).toBe('review');
    expect(group('w-done')).toBe('needs');
    expect(group('w-working')).toBe('working');
  });
  test('pins: own section, except needs-you rows also stay in their group', () => {
    const pins = new Set([workspacePinKey('h1', 'w-idle'), workspacePinKey('h1', 'w-needs')]);
    const list = groupWorkspaces(treeOf(), { pins });
    expect(list.pinned.map((r) => r.workspace.id).sort()).toEqual(['w-idle', 'w-needs']);
    expect(list.groups.find((g) => g.id === 'idle')).toBeUndefined();
    expect(list.groups.find((g) => g.id === 'needs')!.rows.map((r) => r.workspace.id)).toContain('w-needs');
    // A pinned pane pins its workspace.
    const viaPane = groupWorkspaces(treeOf(fixture(), {}, ['h1/p-working']), { pins: new Set(['h1/p-working']) });
    expect(viaPane.pinned.map((r) => r.workspace.id)).toEqual(['w-working']);
  });
  test('filters: query, host, show done', () => {
    const tree = treeOf();
    expect(groupWorkspaces(tree, { query: 'feat' }).groups.flatMap((g) => g.rows).map((r) => r.workspace.id)).toEqual(['w-working']);
    expect(groupWorkspaces(tree, { query: 'review me' }).groups.flatMap((g) => g.rows).map((r) => r.workspace.id)).toEqual(['w-review']);
    expect(groupWorkspaces(tree, { query: 'codex' }).groups.flatMap((g) => g.rows).map((r) => r.workspace.id)).toEqual(['w-working']);
    const noDone = groupWorkspaces(tree, { showDone: false });
    expect(noDone.groups.map((g) => g.id)).toEqual(['needs', 'review', 'working']);
    expect(noDone.hidden).toBe(2);
    expect(groupWorkspaces(tree, { hostFilter: 'other' }).groups).toEqual([]);
    expect(groupWorkspaces(tree, { hostFilter: 'h1' }).groups.length).toBe(5);
  });
  test('hosts without a dashboard contribute nothing; several hosts merge', () => {
    const tree = buildTree([host('h1', fixture()), host('h2', null, 'offline'), host('h3', dashboard())], { pins: new Set(), seenDone: {} });
    const rows = workspaceRows(tree);
    expect(rows.filter((r) => r.host === 'h2')).toEqual([]);
    expect(rows.filter((r) => r.host === 'h3').map((r) => r.hostName)).toEqual(['h3']);
  });
  test('primary pane: needs-you agent, else most recent agent, else first pane', () => {
    const d = dashboard({
      tabs: [tab({ layout: { Split: { dir: 'Vertical', children: [[{ Leaf: { pane: 'p1' } }, 0.3], [{ Leaf: { pane: 'p2' } }, 0.3], [{ Leaf: { pane: 'p3' } }, 0.4]] } } })],
      panes: [pane({ id: 'p1' }), pane({ id: 'p2' }), pane({ id: 'p3' })],
      runs: [run({ id: 'a', pane: 'p2', execution: exec('idle', 10) }), run({ id: 'b', pane: 'p3', execution: exec('idle', 20) })],
    });
    const rows = buildTree([host('h1', d)], { pins: new Set(), seenDone: {} }).all;
    expect(primaryPane(rows)!.pane.id).toBe('p3');
    const urgent = buildTree([host('h1', { ...d, interactions: [interaction({ run: 'a', pane: 'p2' })] })], { pins: new Set(), seenDone: {} }).all;
    expect(primaryPane(urgent)!.pane.id).toBe('p2');
    expect(primaryPane(rows.filter((r) => r.pane.id === 'p1'))!.pane.id).toBe('p1');
    expect(primaryPane([])).toBeNull();
  });
});

describe('selection', () => {
  const rows = workspaceRows(treeOf());
  const sorted = groupWorkspaces(treeOf()).all;
  test('legacy pane links go to their workspace tab', () => {
    expect(resolveLegacy({ name: 'pane', host: 'h1', pane: 'p-working', view: 'term' }, rows)).toEqual(workspaceRoute('h1', 'w-working', { pane: 'p-working' }));
    expect(resolveLegacy({ name: 'pane', host: 'h1', pane: 'p-working', view: 'history' }, rows)).toEqual(workspaceRoute('h1', 'w-working', { pane: 'p-working' }));
    expect(resolveLegacy({ name: 'pane', host: 'h1', pane: 'p-done', view: 'changes' }, rows)).toEqual(workspaceRoute('h1', 'w-done', { pane: 'p-done', panel: 'changes' }));
    expect(resolveLegacy({ name: 'pane', host: 'h1', pane: 'gone', view: 'term' }, rows)).toBeNull();
    expect(workspaceOfPane(rows, 'h2', 'p-done')).toBeNull();
  });
  test('Panes / Focus go to the most urgent workspace; Changes to the last one with the panel', () => {
    const top = sorted[0]!;
    expect(resolveLegacy({ name: 'panes' }, sorted)).toEqual(workspaceRoute('h1', top.workspace.id));
    expect(resolveLegacy({ name: 'focus' }, sorted)).toEqual(workspaceRoute('h1', top.workspace.id));
    expect(resolveLegacy({ name: 'changes' }, sorted, { host: 'h1', ws: 'w-done' })).toEqual(workspaceRoute('h1', 'w-done', { panel: 'changes' }));
    expect(resolveLegacy({ name: 'changes' }, sorted, { host: 'h1', ws: 'gone' })).toEqual(workspaceRoute('h1', top.workspace.id, { panel: 'changes' }));
    expect(resolveLegacy({ name: 'panes' }, [])).toEqual({ name: 'inbox' });
    expect(resolveLegacy({ name: 'inbox' }, sorted)).toBeNull();
    expect(resolveLegacy(workspaceRoute('h1', 'w'), sorted)).toBeNull();
  });
  test('layout breakpoints', () => {
    expect(layoutModeFor(1440)).toBe('wide');
    expect(layoutModeFor(1100)).toBe('wide');
    expect(layoutModeFor(1099)).toBe('mid');
    expect(layoutModeFor(960)).toBe('mid');
    expect(layoutModeFor(959)).toBe('narrow');
    expect(layoutModeFor(390)).toBe('narrow');
  });
  test('panel: explicit wins, the device default applies on wide windows only', () => {
    const r = workspaceRoute('h', 'w');
    expect(effectivePanel(r, { panelOpen: true }, 'wide')).toBe('changes');
    expect(effectivePanel(r, { panelOpen: false }, 'wide')).toBeNull();
    expect(effectivePanel(r, { panelOpen: true }, 'mid')).toBeNull();
    expect(effectivePanel(r, { panelOpen: true }, 'narrow')).toBeNull();
    expect(effectivePanel({ ...r, panel: 'files' }, { panelOpen: false }, 'narrow')).toBe('files');
    expect(effectivePanel({ ...r, panel: 'off' }, { panelOpen: true }, 'wide')).toBeNull();
  });
  test('panel toggle: wide updates the default, narrower windows only the route', () => {
    const r = workspaceRoute('h', 'w', { file: 'a.ts' });
    expect(togglePanelRoute(r, { panelOpen: true }, 'wide')).toEqual({ route: { ...r, file: null, panel: null }, panelOpen: false });
    expect(togglePanelRoute(r, { panelOpen: false }, 'wide')).toEqual({ route: { ...r, file: null, panel: null }, panelOpen: true });
    expect(togglePanelRoute({ ...r, panel: 'off' }, { panelOpen: true }, 'wide')).toEqual({ route: { ...r, file: null, panel: null }, panelOpen: true });
    expect(togglePanelRoute(r, { panelOpen: true }, 'wide', 'files')).toEqual({ route: { ...r, file: null, panel: 'files' }, panelOpen: true });
    expect(togglePanelRoute(r, { panelOpen: true }, 'narrow')).toEqual({ route: { ...r, file: null, panel: 'changes' } });
    expect(togglePanelRoute({ ...r, panel: 'changes' }, { panelOpen: true }, 'narrow')).toEqual({ route: { ...r, file: null, panel: null } });
    expect(togglePanelRoute({ ...r, panel: 'changes' }, { panelOpen: false }, 'mid', 'files')).toEqual({ route: { ...r, file: null, panel: 'files' } });
  });
});
