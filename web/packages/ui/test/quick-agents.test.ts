import { describe, expect, test } from 'bun:test';
import { buildTree } from '../src/lib/tree';
import { agentRows } from '../src/screens/quick-agents';
import { dashboard, host, interaction, pane, run, tab, ws } from './fixtures';

const exec = (value: 'working' | 'idle' | 'error', since_ms: number) => ({ value, since_ms, source: 'structured' as const, confidence: 1, detail: null });

describe('menu-bar agent list', () => {
  test('lists agents only, those that need you first, then the latest change', () => {
    const d = dashboard({
      workspaces: [ws()],
      tabs: [tab()],
      panes: [pane({ id: 'p1' }), pane({ id: 'p2' }), pane({ id: 'p3' }), pane({ id: 'p4' })],
      runs: [
        run({ id: 'r1', pane: 'p1', execution: exec('idle', 5) }),
        run({ id: 'r2', pane: 'p2', execution: exec('working', 9) }),
        run({ id: 'r3', pane: 'p3', execution: exec('working', 2) }),
      ],
      interactions: [interaction({ id: 'i1', run: 'r3', pane: 'p3' })],
    });
    const tree = buildTree([host('h1', d)], { pins: new Set(), seenDone: {} });
    expect(agentRows(tree.hosts[0]!.rows).map((r) => r.run!.id)).toEqual(['r3', 'r2', 'r1']);
  });
});
