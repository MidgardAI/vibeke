import { describe, expect, test } from 'bun:test';
import type { ScreenshotMeta } from '@vibeke/core';
import { ScreenshotStore } from '../src/app/screenshot-store';

const shot = (id: string, at: number): ScreenshotMeta => ({ id, handle: `s${id}`, blob: id, mime: 'image/png', width: 10, height: 10, bytes: 100, created_at_ms: at, label: '' });

function setup() {
  const pending: { params: Record<string, unknown>; resolve(r: unknown): void }[] = [];
  const app = {
    conn: () => ({
      request: (_m: string, params: Record<string, unknown>) => new Promise((resolve) => pending.push({ params, resolve })),
    }),
    manager: { getSnapshot: () => [], subscribe: () => () => {}, subscribeEvents: () => () => {} },
    platform: { clock: { now: () => 1000 } },
  };
  const store = new ScreenshotStore(app as never);
  const reply = (i: number, list: ScreenshotMeta[]) => pending[i]!.resolve({ screenshots: list, count: list.length, total: list.length });
  return { store, pending, reply };
}

describe('screenshot store refresh', () => {
  test('overlapping refreshes run one at a time and the newest list wins', async () => {
    const { store, pending, reply } = setup();
    const off = store.watch('h', 'w');
    expect(pending.length).toBe(1);
    // Two more requests while the first is in flight collapse into one follow-up.
    const a = store.refresh('h', 'w');
    const b = store.refresh('h', 'w');
    expect(pending.length).toBe(1);
    reply(0, [shot('1', 10)]);
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();
    expect(pending.length).toBe(2);
    reply(1, [shot('1', 10), shot('2', 20)]);
    await Promise.all([a, b]);
    expect(pending.length).toBe(2);
    expect(store.get('h', 'w').list.map((s) => s.id)).toEqual(['2', '1']);
    off();
  });
});
