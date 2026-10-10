import { beforeEach, describe, expect, test } from 'bun:test';
import { holdReload, reloadBlockers, resetReloadGuard, subscribeReloadGuard } from '../src/lib/reload-guard';

beforeEach(resetReloadGuard);

describe('reload guard', () => {
  test('is empty by default and lists held reasons in a stable order', () => {
    expect(reloadBlockers()).toEqual([]);
    const a = holdReload('upload');
    const b = holdReload('draft');
    expect(reloadBlockers()).toEqual(['draft', 'upload']);
    a();
    expect(reloadBlockers()).toEqual(['draft']);
    b();
    expect(reloadBlockers()).toEqual([]);
  });
  test('the same reason held twice stays until both release', () => {
    const a = holdReload('sheet');
    const b = holdReload('sheet');
    a();
    expect(reloadBlockers()).toEqual(['sheet']);
    b();
    expect(reloadBlockers()).toEqual([]);
  });
  test('releasing twice does not drop another holder', () => {
    const a = holdReload('upload');
    const b = holdReload('upload');
    a();
    a();
    expect(reloadBlockers()).toEqual(['upload']);
    b();
  });
  test('listeners hear every change and can unsubscribe', () => {
    let n = 0;
    const off = subscribeReloadGuard(() => n++);
    const r = holdReload('draft');
    r();
    expect(n).toBe(2);
    off();
    holdReload('draft')();
    expect(n).toBe(2);
  });
});
