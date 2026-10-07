import { describe, expect, test } from 'bun:test';
import { NotConnectedError, OutcomeUnknownError, RpcError } from '@vibeke/core';
import { classifyError, deliveryView, isSettled, staleInteraction } from '../src/lib/answer';
import { BannerTracker } from '../src/lib/banner';
import { filterFiles, repoTargets, statusLetter } from '../src/lib/changes';
import { badgeCount, hostOfTag, staleTags } from '../src/lib/notify';
import { InboxRetainer, LEAVE_MS, SETTLED_MS } from '../src/lib/retain';
import { buildTree, layoutOrder, neighbours } from '../src/lib/tree';
import { formatRoute, hashFromUrl, parseRoute, type Route } from '../src/router';
import { dashboard, host, interaction, pane, run, tab } from './fixtures';

describe('router', () => {
  test('parses app routes and gateway push deep links', () => {
    expect(parseRoute('')).toEqual({ name: 'home' });
    expect(parseRoute('#/inbox')).toEqual({ name: 'inbox' });
    expect(parseRoute('#/i/h1/int-1')).toEqual({ name: 'interaction', host: 'h1', id: 'int-1', preselect: null });
    expect(parseRoute('#/i/h1/int-1?do=allow')).toMatchObject({ preselect: 'allow' });
    expect(parseRoute('#/i/h1/int-1?do=nuke')).toMatchObject({ preselect: null });
    expect(parseRoute('#/r/h1/run-2')).toEqual({ name: 'run', host: 'h1', run: 'run-2' });
    expect(parseRoute('#/h/h1/p/p%2F1/history')).toEqual({ name: 'pane', host: 'h1', pane: 'p/1', view: 'history' });
    expect(parseRoute('#/nope')).toEqual({ name: 'not_found', path: '/nope' });
  });
  test('pairing link keeps the raw base64url payload', () => {
    expect(parseRoute('#/pair?d=eyJ2Ijox-_abc')).toEqual({ name: 'pair', d: 'eyJ2Ijox-_abc' });
    expect(parseRoute('#/pair')).toEqual({ name: 'pair', d: null });
  });
  test('format round-trips', () => {
    const routes: Route[] = [
      { name: 'home' },
      { name: 'changes' },
      { name: 'settings', section: 'alerts' },
      { name: 'pane', host: 'h', pane: 'p 1', view: 'term' },
      { name: 'pane', host: 'h', pane: 'p', view: 'changes' },
      { name: 'interaction', host: 'h', id: 'i', preselect: 'deny' },
      { name: 'run', host: 'h', run: 'r' },
      { name: 'pair', d: 'abc' },
    ];
    for (const r of routes) expect(parseRoute(formatRoute(r))).toEqual(r);
  });
  test('push urls to hashes', () => {
    expect(hashFromUrl('#/i/h/1')).toBe('#/i/h/1');
    expect(hashFromUrl('/#/inbox')).toBe('#/inbox');
    expect(hashFromUrl('https://app.example/#/r/h/x')).toBe('#/r/h/x');
    expect(hashFromUrl('')).toBe('#/');
  });
});

describe('pane tree', () => {
  const d = dashboard({
    tabs: [tab({ layout: { Split: { dir: 'Vertical', children: [[{ Leaf: { pane: 'p2' } }, 0.5], [{ Leaf: { pane: 'p1' } }, 0.5]] } } })],
    panes: [pane({ id: 'p1' }), pane({ id: 'p2', auto_title: 'vim' })],
    runs: [run({ id: 'r1', pane: 'p1', done_rev: 2 }), run({ id: 'r2', pane: 'p2', execution: { value: 'working', since_ms: 5, source: 'structured', confidence: 1, detail: null } })],
    interactions: [interaction({ pane: 'p2', run: 'r2' })],
  });
  test('layout order, attention, needs-you summary, pins', () => {
    expect(layoutOrder(d.tabs[0]!.layout)).toEqual(['p2', 'p1']);
    const tree = buildTree([host('h1', d)], { pins: new Set(['h1/p1']), seenDone: { 'h1/r1': 1 } });
    const rows = tree.hosts[0]!.workspaces[0]!.tabs[0]!.rows;
    expect(rows.map((r) => r.pane.id)).toEqual(['p2', 'p1']);
    expect(rows[0]!.attention).toBe('interaction');
    expect(rows[1]!.attention).toBe('needs_input'); // finished: done_rev 2 > seen 1
    expect(tree.needYou.map((r) => r.pane.id)).toEqual(['p2', 'p1']);
    expect(tree.pinned.map((r) => r.pane.id)).toEqual(['p1']);
    expect(neighbours(tree, 'h1', 'p2').next?.pane.id).toBe('p1');
    expect(neighbours(tree, 'h1', 'p2').prev).toBeNull();
  });
  test('seen done_rev means idle', () => {
    const tree = buildTree([host('h1', d)], { pins: new Set(), seenDone: { 'h1/r1': 2 } });
    expect(tree.all.find((r) => r.pane.id === 'p1')!.attention).toBe('idle');
  });
});

describe('delivery view', () => {
  const it = interaction();
  test('local phases map to card states', () => {
    expect(deliveryView({ phase: 'sending', label: 'allow', at: 0 }, it)).toBe('sending');
    expect(deliveryView({ phase: 'sent', label: 'allow', at: 0 }, { ...it, status: 'answered', delivery: 'delivering' })).toBe('delivering');
    expect(deliveryView({ phase: 'sent', label: 'allow', at: 0 }, { ...it, status: 'answered', delivery: 'delivered' })).toBe('delivered');
    expect(deliveryView({ phase: 'sent', label: 'allow', at: 0 }, { ...it, status: 'answered', delivery: 'failed' })).toBe('failed');
    expect(deliveryView({ phase: 'unknown', label: 'allow', at: 0 }, it)).toBe('unknown');
    expect(deliveryView({ phase: 'unknown', label: 'allow', at: 0 }, { ...it, status: 'answered', delivery: 'delivered' })).toBe('delivered');
    expect(deliveryView({ phase: 'stale', label: 'allow', at: 0 }, it)).toBe('stale');
    expect(deliveryView(undefined, it)).toBeNull();
    expect(deliveryView(undefined, { ...it, status: 'answered', answered_by: 'tui' })).toBe('answered_elsewhere');
    expect(isSettled('delivered')).toBe(true);
    expect(isSettled('failed')).toBe(false);
  });
  test('error classes', () => {
    const stale = new RpcError('interaction.answer', { code: -32009, message: 'changed', data: { kind: 'stale', details: { interaction: { id: 'i1', status: 'answered' } } } });
    expect(classifyError(stale)).toBe('stale');
    expect(staleInteraction(stale)?.id).toBe('i1');
    expect(classifyError(new OutcomeUnknownError('x', true, 'op', 'closed'))).toBe('unknown');
    expect(classifyError(new NotConnectedError('h'))).toBe('offline');
    expect(classifyError(new RpcError('x', { code: -32003, message: 'no', data: { kind: 'forbidden' } }))).toBe('forbidden');
  });
});

describe('inbox retainer', () => {
  const a = { host_id: 'h', interaction: interaction({ id: 'a' }) };
  const b = { host_id: 'h', interaction: interaction({ id: 'b' }) };
  test('answered elsewhere leaves after the animation', () => {
    const r = new InboxRetainer();
    r.update([a, b], () => undefined, () => undefined, 0);
    const out = r.update([b], () => ({ ...a.interaction, status: 'answered' }), () => undefined, 100);
    expect(out.map((e) => [e.key, e.mode])).toEqual([['h/a', 'leaving'], ['h/b', 'open']]);
    expect(r.update([b], () => undefined, () => undefined, 100 + LEAVE_MS).map((e) => e.key)).toEqual(['h/b']);
  });
  test('own answers stay until delivered, failures stay', () => {
    const r = new InboxRetainer();
    r.update([a], () => undefined, () => undefined, 0);
    const local = { phase: 'sent' as const, label: 'allow', at: 0 };
    let live = { ...a.interaction, status: 'answered' as const, delivery: 'delivering' as const };
    expect(r.update([], () => live, () => local, 10)[0]).toMatchObject({ mode: 'answered' });
    live = { ...live, delivery: 'delivered' as any };
    expect(r.update([], () => live, () => local, 20).length).toBe(1);
    expect(r.update([], () => live, () => local, 20 + SETTLED_MS).length).toBe(0);

    const r2 = new InboxRetainer();
    r2.update([a], () => undefined, () => undefined, 0);
    const failed = { ...a.interaction, status: 'answered' as const, delivery: 'failed' as const };
    expect(r2.update([], () => failed, () => local, 60_000).length).toBe(1);
  });
});

describe('notifications', () => {
  test('stale tags: hosts with nothing open, unpaired hosts, test pushes', () => {
    const h1 = host('h1', dashboard({ interactions: [interaction()] }));
    const h2 = host('h2', dashboard({ interactions: [interaction({ status: 'answered' })] }));
    const h3 = host('h3', null, 'offline');
    expect(hostOfTag('vibeke:h1')).toBe('h1');
    expect(hostOfTag('other')).toBeNull();
    expect(staleTags(['vibeke:h1', 'vibeke:h2', 'vibeke:h3', 'vibeke:gone', 'vibeke:h1:test', 'x'], [h1, h2, h3])).toEqual([
      'vibeke:h2',
      'vibeke:gone',
      'vibeke:h1:test',
    ]);
    expect(badgeCount([h1, h2, h3])).toBe(1);
  });
});

describe('connection banner', () => {
  test('amber after 4 s, red after 15 s, green flash on recovery', () => {
    const b = new BannerTracker();
    const up = host('h', null, 'online');
    const down = host('h', null, 'offline');
    expect(b.update([up], 0).level).toBe('none');
    expect(b.update([down], 1000).level).toBe('none');
    expect(b.update([down], 5100).level).toBe('amber');
    expect(b.update([down], 16_100).level).toBe('red');
    expect(b.update([up], 17_000).level).toBe('green');
    expect(b.update([up], 19_100).level).toBe('none');
  });
  test('revoked is red immediately; short blips never show', () => {
    const b = new BannerTracker();
    expect(b.update([host('h', null, 'revoked')], 0)).toMatchObject({ level: 'red', fatal: ['h'] });
    const c = new BannerTracker();
    c.update([host('h', null, 'connecting')], 0);
    expect(c.update([host('h', null, 'online')], 2000).level).toBe('none');
  });
});

describe('changes', () => {
  const files = [
    { path: 'src/a.ts', x: 'M', y: '.', kind: 'modified', staged: true, binary: false },
    { path: 'src/b.ts', x: '.', y: 'M', kind: 'modified', staged: false, binary: false },
    { path: 'new.txt', x: '?', y: '?', kind: 'untracked', staged: false, binary: false },
    { path: '.env', x: '.', y: 'M', kind: 'modified', staged: false, binary: false, secret: true },
  ];
  test('filters by status and path', () => {
    expect(filterFiles(files, 'staged', '').map((f) => f.path)).toEqual(['src/a.ts']);
    expect(filterFiles(files, 'unstaged', '').map((f) => f.path)).toEqual(['src/b.ts', '.env']);
    expect(filterFiles(files, 'untracked', '').map((f) => f.path)).toEqual(['new.txt']);
    expect(filterFiles(files, 'all', 'SRC/').length).toBe(2);
    expect(files.map(statusLetter)).toEqual(['M', 'M', '?', 'M']);
  });
  test('one status target per working directory', () => {
    const t = repoTargets([
      { host: 'h', pane: 'p1', cwd: '/r', label: '' },
      { host: 'h', pane: 'p2', cwd: '/r', label: '' },
      { host: 'h', pane: 'p3', cwd: null, label: '' },
      { host: 'g', pane: 'p4', cwd: '/r', label: '' },
    ]);
    expect(t.map((x) => x.pane)).toEqual(['p1', 'p4']);
  });
});

describe('share and handoff hosts', () => {
  test('an expired share is not an outage for the banner', () => {
    const b = new BannerTracker();
    expect(b.update([host('h1', null, 'expired')], 0).level).toBe('none');
    expect(b.update([host('h1', null, 'expired')], 60_000).level).toBe('none');
  });
});

describe('answer calls send decision_rev', () => {
  async function model() {
    const { AppModel } = await import('../src/app/model');
    const kv = new Map<string, string>();
    const platform = {
      kv: { get: (k: string) => kv.get(k) ?? null, set: (k: string, v: string) => void kv.set(k, v), remove: (k: string) => void kv.delete(k) },
      clock: { now: () => 0, setTimeout: () => 0, clearTimeout: () => {} },
      defaultDeviceName: 'test',
    };
    const app = new AppModel(platform as never);
    const calls: [string, Record<string, unknown>][] = [];
    let fail: unknown = null;
    const conn = {
      request: async (m: string, p: Record<string, unknown>) => {
        calls.push([m, p]);
        if (fail) throw fail;
        if (m === 'interaction.answer_batch') return { results: (p.items as { interaction: string }[]).map((x) => ({ interaction: x.interaction, ok: true, result: { delivery: { channel: 'native' } } })) };
        return { interaction: {}, delivery: { channel: 'native' } };
      },
      refresh: async () => {},
    };
    (app as unknown as { conn: () => unknown }).conn = () => conn;
    return { app, calls, setFail: (e: unknown) => (fail = e) };
  }

  test('single answers (approval, question) carry the card revision', async () => {
    const { app, calls } = await model();
    await app.answer('h1', interaction({ id: 'a', decision_rev: 4 }), { decision: 'allow' }, 'allow');
    await app.answer('h1', interaction({ id: 'q', kind: 'question', decision_rev: 9 }), { choices: { q1: ['o1'] }, text: 'hi' }, 'answer');
    expect(calls).toEqual([
      ['interaction.answer', { interaction: 'a', decision: 'allow', decision_rev: 4 }],
      ['interaction.answer', { interaction: 'q', choices: { q1: ['o1'] }, text: 'hi', decision_rev: 9 }],
    ]);
    expect(app.answers.get('h1/a')?.phase).toBe('sent');
  });

  test('batch items each carry their revision', async () => {
    const { app, calls } = await model();
    const items = [interaction({ id: 'a', decision_rev: 2 }), interaction({ id: 'b', decision_rev: 6 })].map((i) => ({ host_id: 'h1', interaction: i, run: null }));
    const r = await app.answerBatch({ fingerprint: 'f', host_id: 'h1', items, risk: 'low' } as never, 'allow');
    expect(r).toEqual({ ok: 2, total: 2 });
    expect(calls[0]).toEqual(['interaction.answer_batch', { items: [{ interaction: 'a', decision_rev: 2 }, { interaction: 'b', decision_rev: 6 }], decision: 'allow' }]);
  });

  test('stale marks the card stale and refreshes; a card without a revision never sends', async () => {
    const { app, calls, setFail } = await model();
    setFail(new RpcError('interaction.answer', { code: -32009, message: 'changed', data: { kind: 'stale' } }));
    await app.answer('h1', interaction({ id: 's', decision_rev: 1 }), { decision: 'deny' }, 'deny');
    expect(app.answers.get('h1/s')?.phase).toBe('stale');
    setFail(null);
    const before = calls.length;
    await app.answer('h1', interaction({ id: 'n', decision_rev: undefined as unknown as number }), { decision: 'allow' }, 'allow');
    expect(calls.length).toBe(before);
    expect(app.answers.get('h1/n')?.phase).toBe('error');
  });
});
