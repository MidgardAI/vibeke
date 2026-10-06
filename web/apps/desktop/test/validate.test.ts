import { describe, expect, test } from 'bun:test';
import * as v from '../src/main/validate';

describe('IPC validators', () => {
  test('sender origin: only the bundled app (and the dev server when configured)', () => {
    const trusted = ['app://vibeke'];
    expect(v.isTrustedUrl('app://vibeke/index.html?surface=quick#/inbox', trusted)).toBe(true);
    expect(v.isTrustedUrl('app://evil/index.html', trusted)).toBe(false);
    expect(v.isTrustedUrl('https://vibeke/index.html', trusted)).toBe(false);
    expect(v.isTrustedUrl('file:///Applications/Vibeke.app/index.html', trusted)).toBe(false);
    expect(v.isTrustedUrl('', trusted)).toBe(false);
    expect(v.isTrustedUrl(undefined, trusted)).toBe(false);
    expect(v.isTrustedUrl('http://127.0.0.1:5174/index.html', [...trusted, 'http://127.0.0.1:5174'])).toBe(true);
    expect(v.isTrustedUrl('http://127.0.0.1:5175/', [...trusted, 'http://127.0.0.1:5174'])).toBe(false);
  });

  test('methods: allow-list only (no connection management from renderers)', () => {
    expect(v.method('interaction.answer')).toBe('interaction.answer');
    for (const bad of ['hello', 'events.subscribe', 'client.visibility', 'push.subscribe', 'pair.claim', '__proto__', 1, null]) {
      expect(() => v.method(bad)).toThrow(v.IpcValidationError);
    }
  });

  test('host ids and params', () => {
    expect(v.hostId('abcdefghijklmnopqrstuvwxyz')).toBe('abcdefghijklmnopqrstuvwxyz');
    expect(() => v.hostId('../x')).toThrow();
    expect(() => v.hostId('')).toThrow();
    expect(v.params(undefined)).toEqual({});
    expect(v.params({ a: [1, 'x', { b: null }] })).toEqual({ a: [1, 'x', { b: null }] });
    expect(() => v.params([])).toThrow();
    expect(() => v.params(new Date())).toThrow();
    expect(() => v.params({ n: Number.NaN })).toThrow();
    expect(() => v.params({ big: 'x'.repeat(v.MAX_PARAMS + 1) })).toThrow(/too large/);
    let deep: Record<string, unknown> = {};
    const root = deep;
    for (let i = 0; i < 40; i++) deep = (deep.x = {}) as Record<string, unknown>;
    expect(() => v.params(root)).toThrow(/deep/);
  });

  test('request options', () => {
    expect(v.requestOpts(undefined)).toEqual({});
    expect(v.requestOpts({ timeoutMs: 90_000 })).toEqual({ timeoutMs: 90_000 });
    expect(() => v.requestOpts({ timeoutMs: 1 })).toThrow();
    expect(() => v.requestOpts({ mutating: false })).toThrow();
  });

  test('external URLs: http(s) only, no credentials', () => {
    expect(v.externalUrl('https://example.com/a?b#c')).toBe('https://example.com/a?b#c');
    for (const bad of ['javascript:alert(1)', 'file:///etc/passwd', 'vibeke://pair?d=x', 'https://u:p@example.com', 'smb://x', 42]) {
      expect(() => v.externalUrl(bad)).toThrow();
    }
  });

  test('window ops and hashes', () => {
    expect(v.windowOp({ op: 'pop-out', host: 'h1', pane: 'p1' })).toEqual({ op: 'pop-out', host: 'h1', pane: 'p1' });
    expect(v.windowOp({ op: 'open-main', hash: '#/i/h/1?do=allow' })).toEqual({ op: 'open-main', hash: '#/i/h/1?do=allow' });
    expect(() => v.windowOp({ op: 'open-main', hash: 'https://x' })).toThrow();
    expect(() => v.windowOp({ op: 'exec' })).toThrow();
    expect(() => v.windowOp('close')).toThrow();
  });

  test('accelerators need a modifier and a known key', () => {
    expect(v.accelerator('Alt+CommandOrControl+V')).toBe('Alt+CommandOrControl+V');
    expect(v.accelerator('')).toBe('');
    for (const bad of ['V', 'Alt+Alt+V', 'Hyper+V', 'Alt+VV', 'Command+', 42]) expect(() => v.accelerator(bad)).toThrow();
  });

  test('settings patches', () => {
    expect(v.settingsPatch({ openAtLogin: true, shortcut: 'Shift+Alt+A' })).toEqual({ openAtLogin: true, shortcut: 'Shift+Alt+A' });
    // The executable and the update feed are never renderer-settable (picker / packaged feed).
    expect(() => v.settingsPatch({ vibekePath: '/usr/local/bin/vibeke' } as never)).toThrow();
    expect(() => v.settingsPatch({ updateFeed: 'https://evil.example/feed' } as never)).toThrow();
    // settings.json (written by main) may hold the picked path; a retired updateFeed is dropped.
    expect(v.storedSetting('vibekePath', '/usr/local/bin/vibeke')).toEqual({ vibekePath: '/usr/local/bin/vibeke' });
    expect(() => v.storedSetting('vibekePath', 'vibeke')).toThrow();
    expect(() => v.storedSetting('updateFeed', 'https://x')).toThrow();
    expect(() => v.settingsPatch({ unknown: 1 })).toThrow();
    expect(() => v.settingsPatch({ notifications: 'yes' })).toThrow();
  });

  test('pairing inputs', () => {
    expect(v.linkText('  vibeke://pair?d=abc ')).toBe('vibeke://pair?d=abc');
    expect(() => v.linkText('')).toThrow();
    expect(() => v.linkText('a\u0000b')).toThrow();
    expect(v.pairToken('0f8c4c4e-1b2a-4c8e-9a6e-3c2b1a0f9e8d')).toBeTruthy();
    expect(() => v.pairToken('x')).toThrow();
  });
});
