import { describe, expect, test } from 'bun:test';
import type { Preview } from '@vibeke/core';
import { PreviewViewRegistry, type Timers } from '../src/lib/preview-view-registry';
import {
  collapsePreviewRoute,
  expandPreviewRoute,
  livePreviewCount,
  orderPreviews,
  previewKey,
  previewLabel,
  selectPreviewRoute,
  selectedPreview,
  showsExpandedPreview,
  workspacePreviews,
  hostPreviews,
  panelPreviews,
} from '../src/lib/previews';
import type { PreviewViewApi, PreviewViewState } from '../src/platform';
import { formatRoute, parseRoute, workspaceRoute } from '../src/router';
import type { WorkspaceRow } from '../src/lib/workspaces';

const pv = (id: string, status: string, port: number, o: Partial<Preview> = {}): Preview => ({
  id,
  handle: '',
  pane: 'p1',
  port,
  path: '/',
  label: null,
  url: `http://localhost:${port}/`,
  status,
  ...o,
});

const rowOf = (...panes: string[]) => ({ panes: panes.map((id) => ({ pane: { id } })), task: null }) as unknown as Pick<WorkspaceRow, 'panes' | 'task'>;
const withTask = (task: string, ...panes: string[]) => ({ ...rowOf(...panes), task: { id: task } }) as unknown as Pick<WorkspaceRow, 'panes' | 'task'>;

describe('previews list', () => {
  test('live and declared first, then stopped ones, then suggestions; by port within a status', () => {
    const list = [pv('s1', 'suggested', 3000), pv('d1', 'down', 4000), pv('u2', 'up', 5173), pv('dc', 'declared', 8080), pv('u1', 'up', 3001)];
    expect(orderPreviews(list).map((v) => v.id)).toEqual(['u1', 'u2', 'dc', 'd1', 's1']);
  });
  test("only the workspace's panes, never gone ones", () => {
    const list = [pv('a', 'up', 1), pv('b', 'gone', 2), pv('c', 'suggested', 3), pv('d', 'up', 4, { pane: 'other' }), pv('e', 'up', 5, { pane: null })];
    expect(workspacePreviews(rowOf('p1'), list).map((v) => v.id)).toEqual(['a', 'c']);
    expect(workspacePreviews(rowOf('p1'), undefined)).toEqual([]);
  });
  test("a preview of the workspace's task belongs to it without a pane", () => {
    const list = [pv('t', 'up', 1, { pane: null, task: 'k1' }), pv('u', 'up', 2, { pane: null, task: 'k2' })];
    expect(workspacePreviews(withTask('k1', 'p9'), list).map((v) => v.id)).toEqual(['t']);
  });
  test("the host's other previews are those no workspace claims; limited devices get none", () => {
    const list = [pv('a', 'up', 1), pv('t', 'up', 2, { pane: null, task: 'k1' }), pv('o', 'declared', 3, { pane: null }), pv('g', 'gone', 4, { pane: null }), pv('x', 'up', 5, { pane: 'closed' })];
    const rows = [rowOf('p1'), withTask('k1', 'p2')];
    expect(hostPreviews(rows, list).map((v) => v.id)).toEqual(['x', 'o']);
    const r = panelPreviews(rows[0], rows, list, false);
    expect(r.own.map((v) => v.id)).toEqual(['a']);
    expect(r.all.map((v) => v.id)).toEqual(['a', 'x', 'o']);
    expect(panelPreviews(rows[0], rows, list, true).other).toEqual([]);
  });
  test('the header counts only previews that answer', () => {
    expect(livePreviewCount([pv('a', 'up', 1), pv('b', 'declared', 2), pv('c', 'suggested', 3), pv('d', 'up', 4)])).toBe(2);
  });
  test('labels and keys', () => {
    expect(previewLabel(pv('a', 'up', 3000))).toBe('localhost:3000');
    expect(previewLabel(pv('a', 'up', 3000, { url: 'http://localhost:3000/app' }))).toBe('localhost:3000/app');
    expect(previewLabel(pv('a', 'up', 3000, { label: 'web' }))).toBe('web');
    expect(previewKey('h1', pv('a', 'up', 1))).toBe('h1::a');
    expect(previewKey('h1', pv('a', 'up', 1, { machine: 'devbox' }))).toBe('h1:devbox:a');
  });
});

describe('preview routes', () => {
  const list = [pv('v1', 'up', 3000), pv('v2', 'up', 3001)];
  test('select, expand, collapse', () => {
    const base = workspaceRoute('h', 'w', { panel: 'previews' });
    const sel = selectPreviewRoute(base, 'v1');
    expect(selectedPreview(sel, list)?.id).toBe('v1');
    expect(sel.expanded).toBe(false);
    const big = expandPreviewRoute(sel);
    expect(big).toMatchObject({ preview: 'v1', expanded: true, panel: 'off' });
    expect(showsExpandedPreview(big, list)).toBe(true);
    // Another preview picked while expanded stays in the centre.
    expect(selectPreviewRoute(big, 'v2')).toMatchObject({ preview: 'v2', expanded: true });
    // Back to the list ends the expanded view.
    expect(selectPreviewRoute(big, null)).toMatchObject({ preview: null, expanded: false });
    expect(collapsePreviewRoute(big)).toMatchObject({ preview: 'v1', expanded: false, panel: 'previews' });
    // Nothing selected: nothing to expand; a gone preview is not shown.
    expect(expandPreviewRoute(base)).toBe(base);
    expect(showsExpandedPreview({ preview: 'gone', expanded: true }, list)).toBe(false);
  });
  test('the hash carries the panel, the preview and the expanded state', () => {
    const r = workspaceRoute('h', 'w', { pane: 'p', panel: 'off', preview: 'v1', expanded: true });
    expect(formatRoute(r)).toBe('#/w/h/w/t/p?panel=off&preview=v1&expand=1');
    expect(parseRoute(formatRoute(r))).toEqual(r);
    expect(parseRoute('#/w/h/w?panel=previews&preview=v2')).toMatchObject({ panel: 'previews', preview: 'v2', expanded: false });
    // expand without a preview means nothing.
    expect(parseRoute('#/w/h/w?expand=1')).toMatchObject({ preview: null, expanded: false });
  });
  test('older preview tab links open the preview in the Previews panel', () => {
    expect(parseRoute('#/w/h/w/t/p?show=preview:v1')).toMatchObject({ show: null, preview: 'v1', panel: 'previews', expanded: false });
    expect(workspaceRoute('h', 'w', { pane: 'p', show: 'preview:v1' })).toMatchObject({ show: null, preview: 'v1', panel: 'previews' });
    expect(workspaceRoute('h', 'w', { show: 'term' })).toMatchObject({ show: 'term', preview: null });
  });
});

// ---- the embedded view registry ---------------------------------------------------------------

class FakeTimers implements Timers {
  now = 0;
  private next = 1;
  private queue = new Map<number, { at: number; fn: () => void }>();
  set(fn: () => void, ms: number): unknown {
    const id = this.next++;
    this.queue.set(id, { at: this.now + ms, fn });
    return id;
  }
  clear(h: unknown): void {
    this.queue.delete(h as number);
  }
  advance(ms: number): void {
    this.now += ms;
    for (const [id, t] of [...this.queue]) {
      if (t.at <= this.now) {
        this.queue.delete(id);
        t.fn();
      }
    }
  }
}

function fakeApi() {
  const log: string[] = [];
  let n = 0;
  let fail = false;
  const pending: ((v: { view: string }) => void)[] = [];
  let hold = false;
  let stateCb: ((s: PreviewViewState) => void) | null = null;
  const api: Pick<PreviewViewApi, 'open' | 'close' | 'bounds' | 'onState'> = {
    open: (p) => {
      log.push(`open ${p.hostId}/${p.machine}/${p.url}`);
      if (fail) return Promise.reject(new Error('no tunnel'));
      const view = `v${++n}`;
      if (hold) return new Promise((r) => pending.push(() => r({ view })));
      return Promise.resolve({ view });
    },
    close: (v) => void log.push(`close ${v}`),
    bounds: (v, r, visible) => void log.push(`bounds ${v} ${r.width}x${r.height} ${visible ? 'shown' : 'hidden'}`),
    onState: (cb) => {
      stateCb = cb;
      return () => {
        stateCb = null;
      };
    },
  };
  return {
    api,
    log,
    failNext: (v: boolean) => (fail = v),
    holdOpens: (v: boolean) => (hold = v),
    resolvePending: () => pending.splice(0).forEach((r) => r({ view: '' })),
    push: (s: PreviewViewState) => stateCb?.(s),
    watching: () => !!stateCb,
  };
}

const tick = () => new Promise((r) => setTimeout(r, 0));
const params = { hostId: 'h1', machine: '', url: 'http://localhost:3000/' };
const rect = (w: number, h: number) => ({ x: 0, y: 0, width: w, height: h });

describe('preview view registry', () => {
  test('a placeholder that takes over within the grace time keeps the same view', async () => {
    const f = fakeApi();
    const timers = new FakeTimers();
    const reg = new PreviewViewRegistry(f.api, timers, 1000);
    const releasePanel = reg.claim('k', params);
    await tick();
    expect(reg.get('k')?.view).toBe('v1');
    reg.bounds('k', rect(400, 600), true);
    // Expand: the panel's placeholder goes, the centre's comes in the same render.
    releasePanel();
    const releaseCentre = reg.claim('k', params);
    reg.bounds('k', rect(1200, 800), true);
    timers.advance(5000);
    expect(f.log).toEqual(['open h1//http://localhost:3000/', 'bounds v1 400x600 shown', 'bounds v1 400x600 hidden', 'bounds v1 1200x800 shown']);
    releaseCentre();
    releaseCentre(); // idempotent
    timers.advance(999);
    expect(f.log.at(-1)).toBe('bounds v1 1200x800 hidden');
    timers.advance(1);
    expect(f.log.at(-1)).toBe('close v1');
    expect(reg.get('k')).toBeNull();
    expect(f.watching()).toBe(false);
  });

  test('two placeholders at once share one view; it closes after the last one goes', async () => {
    const f = fakeApi();
    const timers = new FakeTimers();
    const reg = new PreviewViewRegistry(f.api, timers, 1000);
    const a = reg.claim('k', params);
    const b = reg.claim('k', params);
    await tick();
    a();
    timers.advance(2000);
    expect(f.log.filter((l) => l.startsWith('close'))).toEqual([]);
    b();
    timers.advance(1000);
    expect(f.log.filter((l) => l.startsWith('open') || l.startsWith('close'))).toEqual(['open h1//http://localhost:3000/', 'close v1']);
  });

  test('a view that opens after it was given up is closed at once', async () => {
    const f = fakeApi();
    const timers = new FakeTimers();
    const reg = new PreviewViewRegistry(f.api, timers, 1000);
    f.holdOpens(true);
    reg.claim('k', params)();
    timers.advance(1000);
    expect(reg.get('k')).toBeNull();
    f.resolvePending();
    await tick();
    expect(f.log).toEqual(['open h1//http://localhost:3000/', 'close v1']);
  });

  test('a failed open is shown, and the next placeholder tries again', async () => {
    const f = fakeApi();
    const reg = new PreviewViewRegistry(f.api, new FakeTimers(), 1000);
    f.failNext(true);
    const release = reg.claim('k', params);
    await tick();
    expect(reg.get('k')?.error).toBeInstanceOf(Error);
    release();
    f.failNext(false);
    reg.claim('k', params);
    await tick();
    expect(reg.get('k')).toMatchObject({ view: 'v1', error: null });
    expect(f.log.filter((l) => l.startsWith('open')).length).toBe(2);
  });

  test('pushed state is kept per view, so a new placeholder starts with it', async () => {
    const f = fakeApi();
    const reg = new PreviewViewRegistry(f.api, new FakeTimers(), 1000);
    let changes = 0;
    reg.subscribe(() => changes++);
    reg.claim('a', params);
    reg.claim('b', { ...params, url: 'http://localhost:4000/' });
    await tick();
    const state: PreviewViewState = { view: 'v2', url: 'http://localhost:4000/x', title: 'X', loading: false, canGoBack: true, canGoForward: false };
    f.push(state);
    expect(reg.get('b')?.state).toEqual(state);
    expect(reg.get('a')?.state).toBeNull();
    const before = reg.get('a');
    f.push({ ...state, view: 'v9' });
    expect(reg.get('a')).toBe(before);
    expect(changes).toBeGreaterThan(0);
  });
});
