import { describe, expect, test } from 'bun:test';
import {
  MAX_VIEW_OVERRIDES,
  agentViewCommands,
  agentViewFor,
  hasViewOverride,
  otherView,
  paneTabId,
  showBody,
  showFor,
  tabBody,
  withViewOverride,
} from '../src/lib/agent-view';
import { DEFAULT_PREFS, PrefsStore, parsePrefs, type KV } from '../src/lib/prefs';
import { SHORTCUTS, shortcutFor } from '../src/lib/shortcuts';
import { buildTree } from '../src/lib/tree';
import { groupWorkspaces } from '../src/lib/workspaces';
import { formatRoute, parseRoute } from '../src/router';
import { resolveLegacy } from '../src/app/selection';
import { workspaceTabs } from '../src/screens/workspace/tab-strip';
import { dashboard, host, pane, run, tab, ws } from './fixtures';

function memKV(init: Record<string, string> = {}): KV & { data: Record<string, string> } {
  const data = { ...init };
  return { data, get: (k) => data[k] ?? null, set: (k, v) => void (data[k] = v), remove: (k) => void delete data[k] };
}

describe('agent view prefs', () => {
  test('default is conversation, no overrides', () => {
    expect(DEFAULT_PREFS.agentView).toBe('conversation');
    expect(parsePrefs(null).agentView).toBe('conversation');
    expect(parsePrefs(null).agentViews).toEqual({});
  });

  test('parse keeps valid values and drops junk', () => {
    const p = parsePrefs(JSON.stringify({ agentView: 'terminal', agentViews: { 'h1/w1': 'conversation', 'h1/w2': 'terminal', 'h1/w3': 'tui', 'h1/w4': 3 } }));
    expect(p.agentView).toBe('terminal');
    expect(p.agentViews).toEqual({ 'h1/w1': 'conversation', 'h1/w2': 'terminal' });
    expect(parsePrefs(JSON.stringify({ agentView: 'raw', agentViews: [] })).agentView).toBe('conversation');
    expect(parsePrefs(JSON.stringify({ agentView: 'raw', agentViews: [] })).agentViews).toEqual({});
  });

  test('override wins over the default; clearing it falls back to the default', () => {
    const prefs = { agentView: 'conversation' as const, agentViews: { 'h1/w1': 'terminal' as const } };
    expect(agentViewFor(prefs, 'h1', 'w1')).toBe('terminal');
    expect(agentViewFor(prefs, 'h1', 'w2')).toBe('conversation');
    expect(agentViewFor(prefs, 'h2', 'w1')).toBe('conversation');
    expect(hasViewOverride(prefs, 'h1', 'w1')).toBe(true);
    expect(hasViewOverride(prefs, 'h1', 'w2')).toBe(false);
    // A changed default applies to workspaces without an override only.
    const terminalDefault = { ...prefs, agentView: 'terminal' as const, agentViews: { 'h1/w1': 'conversation' as const } };
    expect(agentViewFor(terminalDefault, 'h1', 'w2')).toBe('terminal');
    expect(agentViewFor(terminalDefault, 'h1', 'w1')).toBe('conversation');
  });

  test('the store sets, persists and clears a workspace override', () => {
    const kv = memKV();
    const store = new PrefsStore(kv);
    store.setWorkspaceView('h1', 'w1', 'terminal');
    expect(agentViewFor(store.get(), 'h1', 'w1')).toBe('terminal');
    expect(parsePrefs(kv.data['vibeke.prefs']!).agentViews).toEqual({ 'h1/w1': 'terminal' });
    store.setWorkspaceView('h1', 'w1', null);
    expect(store.get().agentViews).toEqual({});
    expect(agentViewFor(store.get(), 'h1', 'w1')).toBe('conversation');
  });

  test('overrides are capped, most recent kept', () => {
    let m = {};
    for (let i = 0; i < MAX_VIEW_OVERRIDES + 5; i++) m = withViewOverride(m, 'h', `w${i}`, 'terminal');
    expect(Object.keys(m)).toHaveLength(MAX_VIEW_OVERRIDES);
    expect('h/w0' in m).toBe(false);
    expect(`h/w${MAX_VIEW_OVERRIDES + 4}` in m).toBe(true);
    // Setting an existing one moves it to the newest end.
    m = withViewOverride(m, 'h', 'w5', 'conversation');
    expect(Object.keys(m).at(-1)).toBe('h/w5');
  });
});

describe('agent view tabs and ?show=', () => {
  test('show values', () => {
    expect(showBody('term')).toBe('terminal');
    expect(showBody('conversation')).toBe('conversation');
    expect(showBody(null)).toBeNull();
    expect(showBody('preview:x')).toBeNull();
    expect(showFor('terminal', 'conversation')).toBe('term');
    expect(showFor('conversation', 'terminal')).toBe('conversation');
    expect(showFor('terminal', 'terminal')).toBeNull();
    expect(otherView('terminal')).toBe('conversation');
  });

  test('the primary tab shows the chosen view; ?show= picks the other one', () => {
    // Conversation view: no show → primary (conversation); term → secondary terminal.
    expect(paneTabId('p1', true, null, 'conversation')).toBe('a:p1');
    expect(paneTabId('p1', true, 'term', 'conversation')).toBe('t:p1');
    expect(paneTabId('p1', true, 'conversation', 'conversation')).toBe('a:p1');
    // Terminal view: no show → primary (terminal); conversation → secondary conversation.
    expect(paneTabId('p1', true, null, 'terminal')).toBe('a:p1');
    expect(paneTabId('p1', true, 'term', 'terminal')).toBe('a:p1');
    expect(paneTabId('p1', true, 'conversation', 'terminal')).toBe('c:p1');
    // Shells are always the terminal; previews win.
    expect(paneTabId('p2', false, null, 'conversation')).toBe('t:p2');
    expect(paneTabId('p2', false, 'conversation', 'terminal')).toBe('t:p2');
    expect(paneTabId('p1', true, 'preview:v1', 'terminal')).toBe('p:v1');
    // What each tab shows.
    expect(tabBody('a:p1', 'terminal')).toBe('terminal');
    expect(tabBody('a:p1', 'conversation')).toBe('conversation');
    expect(tabBody('t:p1', 'terminal')).toBe('terminal');
    expect(tabBody('c:p1', 'terminal')).toBe('conversation');
    expect(tabBody('p:v1', 'terminal')).toBe('preview');
  });

  test('workspace tabs: one per agent (the toggle picks its view), then shells', () => {
    const d = dashboard({
      workspaces: [ws({ id: 'w1' })],
      tabs: [tab({ id: 't1', layout: { Split: { dir: 'Horizontal', children: [[{ Leaf: { pane: 'p1' } }, 0.5], [{ Leaf: { pane: 'p2' } }, 0.5]] } } })],
      panes: [pane({ id: 'p1' }), pane({ id: 'p2', auto_title: 'zsh' })],
      runs: [run({ id: 'r1', pane: 'p1' })],
    });
    const row = groupWorkspaces(buildTree([host('h1', d)], { pins: new Set(), seenDone: {} }), { pins: new Set() }).all[0]!;
    expect(workspaceTabs(row, []).map((x) => [x.id, x.kind])).toEqual([
      ['a:p1', 'agent'],
      ['t:p2', 'term'],
    ]);
  });

  test('routes carry ?show=conversation, and pane links keep show into the workspace', () => {
    const r = parseRoute('#/w/h1/w1/t/p1?show=conversation');
    expect(r).toMatchObject({ name: 'workspace', pane: 'p1', show: 'conversation' });
    expect(formatRoute(r)).toBe('#/w/h1/w1/t/p1?show=conversation');
    const p = parseRoute('#/h/h1/p/p1?show=term');
    expect(p).toEqual({ name: 'pane', host: 'h1', pane: 'p1', view: 'term', show: 'term' });
    expect(formatRoute(p)).toBe('#/h/h1/p/p1?show=term');
    expect(parseRoute('#/h/h1/p/p1')).toEqual({ name: 'pane', host: 'h1', pane: 'p1', view: 'term' });
    const d = dashboard({ workspaces: [ws({ id: 'w1' })], tabs: [tab({ id: 't1' })], panes: [pane({ id: 'p1' })], runs: [run({ id: 'r1', pane: 'p1' })] });
    const rows = groupWorkspaces(buildTree([host('h1', d)], { pins: new Set(), seenDone: {} }), { pins: new Set() }).all;
    expect(resolveLegacy(p, rows)).toMatchObject({ name: 'workspace', workspace: 'w1', pane: 'p1', show: 'term' });
  });
});

describe('agent view commands', () => {
  const k = (key: string, o: Partial<{ metaKey: boolean; ctrlKey: boolean; shiftKey: boolean; altKey: boolean }> = {}) => ({ key, metaKey: false, ctrlKey: false, altKey: false, shiftKey: false, ...o });
  const ctx = (o: Partial<{ mac: boolean; typing: boolean; dialog: boolean; onControl: boolean }> = {}) => ({ mac: true, typing: false, dialog: false, onControl: false, ...o });

  test('⌘⇧T / Ctrl+Shift+T toggles, also while typing; listed in the cheat sheet', () => {
    expect(shortcutFor(k('T', { metaKey: true, shiftKey: true }), ctx())).toEqual({ type: 'agentView' });
    expect(shortcutFor(k('t', { metaKey: true, shiftKey: true }), ctx({ typing: true }))).toEqual({ type: 'agentView' });
    expect(shortcutFor(k('T', { ctrlKey: true, shiftKey: true }), ctx({ mac: false }))).toEqual({ type: 'agentView' });
    expect(shortcutFor(k('t', { metaKey: true }), ctx())).toBeNull();
    expect(SHORTCUTS.some((s) => s.what === 'agentView' && s.keys.includes('mod+shift+t'))).toBe(true);
  });

  test('palette: both views, the flip marked; "use default" only when overridden', () => {
    const base = { agentView: 'conversation' as const, agentViews: {} };
    expect(agentViewCommands(base, 'h1', 'w1')).toEqual([
      { id: 'view-conversation', request: 'conversation', flip: false },
      { id: 'view-terminal', request: 'terminal', flip: true },
    ]);
    const over = { agentView: 'conversation' as const, agentViews: { 'h1/w1': 'terminal' as const } };
    expect(agentViewCommands(over, 'h1', 'w1')).toEqual([
      { id: 'view-conversation', request: 'conversation', flip: true },
      { id: 'view-terminal', request: 'terminal', flip: false },
      { id: 'view-default', request: 'default', flip: false },
    ]);
  });
});
