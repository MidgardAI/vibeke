import { describe, expect, test } from 'bun:test';
import { buildOverview, feedOf, filterFeed, overviewGroup, tilesOf } from '../src/lib/overview';
import { parseRoute, formatRoute } from '../src/router';
import { shortcutFor } from '../src/lib/shortcuts';
import { buildTree } from '../src/lib/tree';
import { groupWorkspaces } from '../src/lib/workspaces';
import { dashboard, host, interaction, pane, run, tab, ws } from './fixtures';

const exec = (value: 'idle' | 'working' | 'error' | 'exited', since_ms: number) => ({ value, since_ms, source: 'structured' as const, confidence: 1, detail: null });

/** One workspace (and pane) per entry; `runs` and `interactions` refer to `p<n>`. */
function setup(n: number, extra: NonNullable<Parameters<typeof dashboard>[0]> = {}, status: 'online' | 'offline' = 'online', seenDone: Record<string, number> = {}) {
  const ids = Array.from({ length: n }, (_, i) => i + 1);
  const d = dashboard({
    workspaces: ids.map((i) => ws({ id: `w${i}`, name: `ws${i}`, order: i })),
    tabs: ids.map((i) => tab({ id: `t${i}`, workspace: `w${i}`, layout: { Leaf: { pane: `p${i}` } } })),
    panes: ids.map((i) => pane({ id: `p${i}`, tab: `t${i}`, workspace: `w${i}` })),
    runs: [],
    ...extra,
  });
  const hosts = [host('h1', d, status)];
  const rows = groupWorkspaces(buildTree(hosts, { pins: new Set(), seenDone })).all;
  return { rows, hosts };
}

describe('overview model', () => {
  test('counts follow the sidebar groups; unseen finished work counts as ready to review', () => {
    const { rows, hosts } = setup(4, {
      runs: [
        run({ id: 'r1', pane: 'p1', execution: exec('working', 50) }),
        run({ id: 'r2', pane: 'p2', execution: exec('idle', 40), turns_completed: 2, done_rev: 1 }),
        run({ id: 'r3', pane: 'p3', execution: exec('working', 30) }),
      ],
      interactions: [interaction({ id: 'i1', run: 'r3', pane: 'p3', opened_at_ms: 10 })],
    }, 'online', { 'h1/r2': 0 });
    const ov = buildOverview(rows, hosts);
    expect(ov.counts).toMatchObject({ needs: 1, working: 1, review: 1, idle: 1 });
    expect(ov.needs.map((r) => r.workspace.id)).toEqual(['w3']);
    expect(ov.working.map((r) => r.workspace.id)).toEqual(['w1']);
    expect(ov.review.map((r) => r.workspace.id)).toEqual(['w2']);
    // Tiles: needs, then review, then working; idle is left out.
    expect(ov.active.map((r) => r.workspace.id)).toEqual(['w3', 'w2', 'w1']);
    expect(overviewGroup(ov.review[0]!)).toBe('review');
  });

  test('the feed lists runs that did something, newest first, and skips fresh idle agents', () => {
    const { rows } = setup(4, {
      runs: [
        run({ id: 'r1', pane: 'p1', execution: exec('working', 300), last_tool: 'Edit' }),
        run({ id: 'r2', pane: 'p2', execution: exec('idle', 500), turns_completed: 3, last_message: 'Done.' }),
        run({ id: 'r3', pane: 'p3', execution: exec('error', 100) }),
        run({ id: 'r4', pane: 'p4', execution: exec('idle', 900), turns_completed: 0 }),
      ],
    });
    const feed = feedOf(rows);
    expect(feed.map((e) => [e.key, e.kind])).toEqual([
      ['h1/r2', 'finished'],
      ['h1/r1', 'working'],
      ['h1/r3', 'error'],
    ]);
    expect(filterFeed(feed, 'h1/w1').map((e) => e.key)).toEqual(['h1/r1']);
    expect(filterFeed(feed, null)).toHaveLength(3);
  });

  test('an ended run from the dashboard shows as exited; the newest run of a pane wins', () => {
    const { rows, hosts } = setup(2, {
      runs: [
        run({ id: 'old', pane: 'p1', execution: exec('exited', 50), started_at_ms: 10, ended_at_ms: 60 }),
        run({ id: 'new', pane: 'p1', execution: exec('working', 80), started_at_ms: 70 }),
        run({ id: 'gone', pane: 'p2', execution: exec('exited', 40), started_at_ms: 5, ended_at_ms: 40 }),
      ],
    });
    expect(feedOf(rows, hosts).map((e) => [e.key, e.kind])).toEqual([
      ['h1/new', 'working'],
      ['h1/gone', 'exited'],
    ]);
  });

  test('an agent stopped with an error needs the user without a request; only background tasks count as background', () => {
    const { rows, hosts } = setup(2, {
      runs: [
        run({ id: 'r1', pane: 'p1', execution: exec('error', 10) }),
        run({ id: 'r2', pane: 'p2', execution: { ...exec('idle', 10), value: 'rate_limited' } }),
      ],
    });
    const ov = buildOverview(rows, hosts);
    expect(ov.stopped.map((r) => r.workspace.id)).toEqual(['w1']);
    expect(ov.counts).toMatchObject({ needs: 1, working: 1, background: 0 });
  });

  test('tiles keep the last slot for a "more" tile', () => {
    const { rows } = setup(8, { runs: Array.from({ length: 8 }, (_, i) => run({ id: `r${i + 1}`, pane: `p${i + 1}`, execution: exec('working', i) })) });
    expect(tilesOf(rows, 6)).toMatchObject({ more: 3 });
    expect(tilesOf(rows, 6).tiles).toHaveLength(5);
    expect(tilesOf(rows.slice(0, 6), 6)).toMatchObject({ more: 0 });
    expect(tilesOf(rows.slice(0, 6), 6).tiles).toHaveLength(6);
  });

  test('hosts report status, panes and live agents', () => {
    const { rows, hosts } = setup(2, { runs: [run({ id: 'r1', pane: 'p1' }), run({ id: 'r2', pane: 'p2', execution: exec('exited', 1) })] }, 'offline');
    expect(buildOverview(rows, hosts).hosts).toEqual([{ id: 'h1', name: 'h1', status: 'offline', panes: 2, agents: 1 }]);
  });
});

describe('overview route and shortcut', () => {
  test('#/overview round-trips', () => {
    expect(parseRoute('#/overview')).toEqual({ name: 'overview' });
    expect(formatRoute({ name: 'overview' })).toBe('#/overview');
  });

  test('mod+shift+H opens the overview', () => {
    const ctx = { mac: true, typing: false, dialog: false, onControl: false };
    expect(shortcutFor({ key: 'H', metaKey: true, ctrlKey: false, altKey: false, shiftKey: true }, ctx)).toEqual({ type: 'go', to: 'overview' });
  });
});
