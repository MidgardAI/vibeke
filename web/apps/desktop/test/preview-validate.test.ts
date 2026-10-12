import { describe, expect, test } from 'bun:test';
import { INVOKE, EVENT } from '../src/shared/contract';
import * as v from '../src/main/validate';

const VIEW = 'pv0123456789abcdef01234567';

describe('preview view IPC validators', () => {
  test('channels exist in the bridge allow-lists', () => {
    expect(Object.values(INVOKE)).toEqual(expect.arrayContaining(['vk:preview.view.open', 'vk:preview.view.bounds', 'vk:preview.view.go', 'vk:preview.view.devtools', 'vk:preview.view.snapshot', 'vk:preview.view.close', 'vk:preview.view.annotate']));
    expect(Object.values(EVENT)).toEqual(expect.arrayContaining(['vk:preview.view.state', 'vk:preview.view.pick']));
  });

  test('open: host id, machine and an http(s) URL only', () => {
    expect(v.previewOpen({ hostId: 'HOST1', machine: 'devbox', url: 'http://localhost:5173/' })).toEqual({ hostId: 'HOST1', machine: 'devbox', url: 'http://localhost:5173/' });
    expect(v.previewOpen({ hostId: 'HOST1', url: 'https://app.localhost' })).toEqual({ hostId: 'HOST1', machine: '', url: 'https://app.localhost/' });
    for (const bad of [
      null,
      [],
      { hostId: '../x', machine: '', url: 'http://localhost/' },
      { hostId: 'H', machine: 'a/b', url: 'http://localhost/' },
      { hostId: 'H', machine: 'x'.repeat(200), url: 'http://localhost/' },
      { hostId: 'H', machine: '', url: 'file:///etc/passwd' },
      { hostId: 'H', machine: '', url: 'app://vibeke/index.html' },
      { hostId: 'H', machine: '', url: 'javascript:alert(1)' },
      { hostId: 'H', machine: '', url: 'http://u:p@localhost/' },
      { hostId: 'H', machine: '', url: 'http://localhost/\n' },
      { hostId: 'H', machine: '', url: 'http://localhost/', extra: 1 },
    ]) {
      expect(() => v.previewOpen(bad)).toThrow(v.IpcValidationError);
    }
  });

  test('view ids are the ones main mints', () => {
    expect(v.previewView(VIEW)).toBe(VIEW);
    for (const bad of ['pv', 'pv0123', `${VIEW}0`, 'PV0123456789ABCDEF01234567', 1, null]) expect(() => v.previewView(bad)).toThrow(v.IpcValidationError);
  });

  test('bounds: finite numbers, non-negative size, a visible flag', () => {
    expect(v.previewBounds({ view: VIEW, x: 10.5, y: 48, width: 800, height: 600, visible: true })).toEqual({ view: VIEW, x: 10.5, y: 48, width: 800, height: 600, visible: true });
    for (const bad of [
      { view: VIEW, x: 0, y: 0, width: -1, height: 1, visible: true },
      { view: VIEW, x: Number.NaN, y: 0, width: 1, height: 1, visible: true },
      { view: VIEW, x: 0, y: 0, width: 1e9, height: 1, visible: true },
      { view: VIEW, x: 0, y: 0, width: 1, height: 1, visible: 'yes' },
      { view: VIEW, x: 0, y: 0, width: 1, height: 1 },
      { view: 'nope', x: 0, y: 0, width: 1, height: 1, visible: true },
    ]) {
      expect(() => v.previewBounds(bad)).toThrow(v.IpcValidationError);
    }
  });

  test('go: a URL or one of the actions, not both', () => {
    expect(v.previewGo({ view: VIEW, url: 'http://localhost:3000/x' })).toEqual({ view: VIEW, url: 'http://localhost:3000/x' });
    for (const action of ['back', 'forward', 'reload', 'stop'] as const) expect(v.previewGo({ view: VIEW, action })).toEqual({ view: VIEW, action });
    for (const bad of [
      { view: VIEW },
      { view: VIEW, action: 'print' },
      { view: VIEW, url: 'file:///x' },
      { view: VIEW, url: 'http://localhost/', action: 'reload' },
    ]) {
      expect(() => v.previewGo(bad)).toThrow(v.IpcValidationError);
    }
  });

  test('annotate: a view id and an on/off flag', () => {
    expect(v.previewAnnotate({ view: VIEW, on: true })).toEqual({ view: VIEW, on: true });
    expect(v.previewAnnotate({ view: VIEW, on: false })).toEqual({ view: VIEW, on: false });
    for (const bad of [null, [], { view: VIEW }, { view: VIEW, on: 1 }, { view: 'nope', on: true }, { view: VIEW, on: true, extra: 1 }]) {
      expect(() => v.previewAnnotate(bad)).toThrow(v.IpcValidationError);
    }
  });
});
