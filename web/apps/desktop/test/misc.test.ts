import { describe, expect, test } from 'bun:test';
import { join } from 'node:path';
import { acceleratorFromEvent, acceleratorLabel } from '../src/shared/accelerator';
import { checkExecutable, defaultGatewayDir, discoverVibeke, parsePairOutput, type FsProbe } from '../src/main/local-pair';
import { popoverPosition, restoreBounds } from '../src/main/store';
import { resolveAppPath } from '../src/main/app-path';
import { accelerator } from '../src/main/validate';
import { linkJson, b64 } from '@vibeke/core';

const key = (code: string, m: Partial<{ meta: boolean; ctrl: boolean; alt: boolean; shift: boolean }> = {}) => ({
  code, metaKey: !!m.meta, ctrlKey: !!m.ctrl, altKey: !!m.alt, shiftKey: !!m.shift,
});

describe('shortcut recording', () => {
  test('physical keys, modifiers in Electron form, round-trips through the validator', () => {
    expect(acceleratorFromEvent(key('KeyV', { alt: true, meta: true }), true)).toBe('Alt+Command+V');
    expect(acceleratorFromEvent(key('Digit1', { ctrl: true, shift: true }), false)).toBe('CommandOrControl+Shift+1');
    expect(acceleratorFromEvent(key('Space', { ctrl: true }), true)).toBe('Control+Space');
    expect(acceleratorFromEvent(key('KeyV'), true)).toBeNull();
    expect(acceleratorFromEvent(key('KeyV', { shift: true }), true)).toBeNull();
    expect(acceleratorFromEvent(key('AltLeft', { alt: true }), true)).toBeNull();
    for (const a of ['Alt+Command+V', 'CommandOrControl+Shift+1', 'Control+Space']) expect(accelerator(a)).toBe(a);
  });
  test('labels', () => {
    expect(acceleratorLabel('Alt+CommandOrControl+V', true)).toBe('⌥⌘V');
    expect(acceleratorLabel('Alt+CommandOrControl+V', false)).toBe('Alt+Ctrl+V');
    expect(acceleratorLabel('', true)).toBe('');
  });
});

describe('connect to this Mac', () => {
  const link = { v: 1, relay: 'local:/Users/me/Library/Application Support/vibeke/gateway/gateway.sock', host: 'h'.repeat(26), hk: b64.encode(new Uint8Array(32).fill(1)), pid: 'p1', psk: b64.encode(new Uint8Array(32).fill(2)), exp: 4_000_000_000, name: 'mac' };
  const d = b64.encode(new TextEncoder().encode(linkJson(link)));
  test('parses the CLI JSON (logs before it are ignored)', () => {
    const out = `some log line\n${JSON.stringify({ link, d, pid: 'p1', socket: link.relay.slice(6) })}\n`;
    const r = parsePairOutput(out);
    expect(r.socket).toBe('/Users/me/Library/Application Support/vibeke/gateway/gateway.sock');
    expect(r.link.host).toBe(link.host);
  });
  test('rejects mismatches and non-local links', () => {
    expect(() => parsePairOutput('nothing')).toThrow(/no JSON/);
    expect(() => parsePairOutput(JSON.stringify({ d, pid: 'p1', socket: '/other.sock' }))).toThrow(/disagree/);
    expect(() => parsePairOutput(JSON.stringify({ d, pid: 'p2', socket: link.relay.slice(6) }))).toThrow(/mismatch/);
    const relayD = b64.encode(new TextEncoder().encode(linkJson({ ...link, relay: 'wss://relay' })));
    expect(() => parsePairOutput(JSON.stringify({ d: relayD, pid: 'p1', socket: '/x' }))).toThrow(/non-local/);
  });
  test('finds the CLI: chosen, $VIBEKE_BIN, usual install dirs, then PATH (confirm unexpected)', () => {
    const f = fakeFs({ '/explicit/vibeke': exe(), '/dev/vibeke': exe(), '/a/vibeke': exe(), '/home/me/.local/bin/vibeke': exe(), '/usr/local/bin/vibeke': exe() });
    const o = { home: '/home/me', fs: f, platform: 'darwin' };
    expect(discoverVibeke({ PATH: '/a', VIBEKE_BIN: '/dev/vibeke' }, '/explicit/vibeke', o)).toEqual({ kind: 'found', path: '/explicit/vibeke' });
    expect(discoverVibeke({ PATH: '/a', VIBEKE_BIN: '/dev/vibeke' }, '', o)).toEqual({ kind: 'found', path: '/dev/vibeke' });
    // Known locations win over PATH order.
    expect(discoverVibeke({ PATH: '/a' }, '', o)).toEqual({ kind: 'found', path: '/home/me/.local/bin/vibeke' });
    // Only on PATH, somewhere unusual: the user must confirm it in the picker.
    const g = fakeFs({ '/a/vibeke': exe() });
    expect(discoverVibeke({ PATH: '/a' }, '', { ...o, fs: g })).toEqual({ kind: 'unexpected', path: '/a/vibeke' });
    expect(discoverVibeke({ PATH: '' }, '', { ...o, fs: g })).toEqual({ kind: 'missing' });
  });
  test('relative PATH entries are never searched; a broken chosen path is reported, not skipped', () => {
    const f = fakeFs({ '/cwd/tools/vibeke': exe(), '/usr/local/bin/vibeke': exe() });
    const o = { home: '/home/me', fs: f, platform: 'darwin' };
    expect(discoverVibeke({ PATH: './tools:tools' }, '', { ...o, fs: fakeFs({ '/cwd/tools/vibeke': exe() }) })).toEqual({ kind: 'missing' });
    expect(discoverVibeke({ PATH: '', VIBEKE_BIN: 'tools/vibeke' }, '', o)).toEqual({ kind: 'found', path: '/usr/local/bin/vibeke' });
    expect(discoverVibeke({}, '/gone/vibeke', o)).toMatchObject({ kind: 'invalid', path: '/gone/vibeke' });
    // An installed but tampered-with binary is reported rather than run.
    const w = fakeFs({ '/usr/local/bin/vibeke': exe({ mode: 0o100777 }) });
    expect(discoverVibeke({}, '', { ...o, fs: w })).toMatchObject({ kind: 'invalid', reason: 'is writable by other users' });
  });
  test('executable trust checks', () => {
    const ok = (meta: Partial<Meta> = {}, dir: Partial<Meta> = {}) => checkExecutable('/d/vibeke', fakeFs({ '/d/vibeke': exe(meta) }, { '/d': dir }));
    expect(ok()).toEqual({ ok: true, path: '/d/vibeke' });
    expect(ok({ uid: 0 })).toEqual({ ok: true, path: '/d/vibeke' });
    expect(ok({ uid: 999 })).toMatchObject({ ok: false, reason: 'is owned by another user' });
    expect(ok({ mode: 0o100775 })).toMatchObject({ ok: false, reason: 'is writable by other users' });
    expect(ok({ mode: 0o100644 })).toMatchObject({ ok: false, reason: 'is not executable' });
    expect(ok({ file: false })).toMatchObject({ ok: false, reason: 'is not a regular file' });
    expect(ok({}, { mode: 0o40777 })).toMatchObject({ ok: false, reason: 'is in a directory anyone can write to' });
    expect(ok({}, { uid: 999 })).toMatchObject({ ok: false, reason: 'is in a directory owned by another user' });
    expect(ok({}, { mode: 0o40775 })).toEqual({ ok: true, path: '/d/vibeke' }); // group-writable dir (Homebrew) is fine
    expect(checkExecutable('vibeke', fakeFs({}))).toMatchObject({ ok: false, reason: 'not an absolute path' });
    // Symlinks are judged by their target.
    const linked = fakeFs({ '/real/vibeke': exe() }, {}, { '/bin2/vibeke': '/real/vibeke' });
    expect(checkExecutable('/bin2/vibeke', linked)).toEqual({ ok: true, path: '/real/vibeke' });
  });
  test('gateway dir mirrors vk-gateway', () => {
    expect(defaultGatewayDir({ VIBEKE_GATEWAY_DIR: '/x' }, 'darwin', '/h')).toBe('/x');
    expect(defaultGatewayDir({}, 'darwin', '/h')).toBe('/h/Library/Application Support/vibeke/gateway');
    expect(defaultGatewayDir({ XDG_CONFIG_HOME: '/c' }, 'linux', '/h')).toBe('/c/vibeke/gateway');
    expect(defaultGatewayDir({}, 'linux', '/h')).toBe('/h/.config/vibeke/gateway');
  });
});

describe('windows', () => {
  const display = { x: 0, y: 0, width: 1440, height: 900 };
  test('saved bounds restore only when on a display', () => {
    expect(restoreBounds({ x: 100, y: 100, width: 800, height: 600 }, [display], { width: 380, height: 480 })).toEqual({ x: 100, y: 100, width: 800, height: 600, maximized: false });
    expect(restoreBounds({ x: 3000, y: 100, width: 800, height: 600 }, [display], { width: 380, height: 480 })).toBeNull();
    expect(restoreBounds({ x: 10, y: 10, width: 100, height: 100 }, [display], { width: 380, height: 480 })).toMatchObject({ width: 380, height: 480 });
    expect(restoreBounds(null, [display], { width: 1, height: 1 })).toBeNull();
    expect(restoreBounds({ x: 'a' }, [display], { width: 1, height: 1 })).toBeNull();
  });
  test('popover sits under the tray icon, inside the work area', () => {
    const work = { x: 0, y: 25, width: 1440, height: 875 };
    expect(popoverPosition({ x: 1000, y: 0, width: 24, height: 24 }, { width: 400, height: 580 }, work, 'darwin')).toEqual({ x: 812, y: 29 });
    expect(popoverPosition({ x: 1430, y: 0, width: 24, height: 24 }, { width: 400, height: 580 }, work, 'darwin').x).toBe(1034);
    // Windows taskbar at the bottom → opens upwards.
    expect(popoverPosition({ x: 1000, y: 870, width: 24, height: 30 }, { width: 400, height: 580 }, { x: 0, y: 0, width: 1440, height: 860 }, 'win32').y).toBe(276);
    expect(popoverPosition(null, { width: 400, height: 580 }, work, 'linux')).toEqual({ x: 1028, y: 37 });
  });
  test('app:// paths never escape the renderer dir', () => {
    expect(resolveAppPath('/app/out/renderer', '/')).toBe(join('/app/out/renderer', 'index.html'));
    expect(resolveAppPath('/app/out/renderer', '/assets/x.js')).toBe('/app/out/renderer/assets/x.js');
    expect(resolveAppPath('/app/out/renderer', '/../main/index.cjs')).toBeNull();
    expect(resolveAppPath('/app/out/renderer', '/%2e%2e/main/index.cjs')).toBeNull();
    expect(resolveAppPath('/app/out/renderer', '/a%00b')).toBeNull();
    expect(resolveAppPath('/app/out/renderer', '/%E0%A4%A')).toBeNull();
  });
});

interface Meta {
  mode: number;
  uid: number;
  file: boolean;
}
const ME = 501;
const exe = (m: Partial<Meta> = {}): Meta => ({ mode: 0o100755, uid: ME, file: true, ...m });
/** Files (and optional directory metadata / symlinks); every other directory is the user's 0755. */
function fakeFs(files: Record<string, Meta>, dirs: Record<string, Partial<Meta>> = {}, links: Record<string, string> = {}): FsProbe {
  const meta = (p: string): Meta => {
    const f = files[p];
    if (f) return f;
    if (Object.keys(files).some((k) => k.startsWith(`${p}/`)) || dirs[p]) return { mode: 0o40755, uid: ME, file: false, ...dirs[p] };
    throw new Error('ENOENT');
  };
  return {
    uid: ME,
    realpath: (p) => {
      const r = links[p] ?? p;
      meta(r);
      return r;
    },
    stat: (p) => {
      const m = meta(p);
      return { isFile: () => m.file, isDirectory: () => !m.file, mode: m.mode, uid: m.uid };
    },
  };
}

describe('updates', () => {
  test('only a packaged generic https feed turns updates on', async () => {
    const { packagedFeed } = await import('../src/main/updater-feed');
    expect(packagedFeed('provider: generic\nurl: https://dl.example.com/vibeke\nupdaterCacheDirName: x\n')).toBe('https://dl.example.com/vibeke');
    expect(packagedFeed('provider: generic\nurl: http://dl.example.com\n')).toBeNull();
    expect(packagedFeed('provider: github\nowner: x\n')).toBeNull();
    expect(packagedFeed('')).toBeNull();
  });
});
