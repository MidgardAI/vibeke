import { afterEach, expect, test } from 'bun:test';
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { CacheStore, MAX_ENTRY } from '../src/main/cache';

const dirs: string[] = [];
afterEach(() => { for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true }); });
function setup() {
  const dir = mkdtempSync(join(tmpdir(), 'vk-cache-')); dirs.push(dir);
  const safe = { calls: 0, isEncryptionAvailable: () => true,
    encryptString(s: string) { this.calls++; return Buffer.from(Buffer.from(s).map((b) => b ^ 0x5a)); },
    decryptString(b: Buffer) { return Buffer.from(b.map((v) => v ^ 0x5a)).toString(); },
  };
  return { dir, safe, store: new CacheStore(dir, safe, 'darwin', 10) };
}

test('screens coalesce into one encrypted write and survive a restart', async () => {
  const { dir, safe, store } = setup();
  for (let i = 0; i < 30; i++) await store.set('mirror', 'host', 'host/pane', JSON.stringify({ text: `screen ${i}` }));
  await store.set('dashboard', 'host', 'host', '{"at":1}');
  await store.flush();
  expect(safe.calls).toBe(1);
  expect(readFileSync(store.file).includes(Buffer.from('screen'))).toBe(false);
  expect(statSync(store.file).mode & 0o777).toBe(0o600);
  const again = new CacheStore(dir, safe, 'darwin');
  expect(await again.get('mirror', 'host/pane')).toBe('{"text":"screen 29"}');
  expect(await again.get('dashboard', 'host')).toBe('{"at":1}');
  expect(await again.get('dashboard', 'other')).toBeNull();
});

test('removed and unpaired hosts lose their copies; late writes for them are ignored', async () => {
  const { dir, safe, store } = setup();
  await store.set('mirror', 'a', 'a/p', '"a"');
  await store.set('mirror', 'b', 'b/p', '"b"');
  await store.set('dashboard', 'c', 'c', '"c"');
  await store.removeHost('a');
  expect(await store.get('mirror', 'a/p')).toBeNull();
  await store.retainHosts(['b']);
  expect(await store.get('dashboard', 'c')).toBeNull();
  await store.set('dashboard', 'c', 'c', '"late"');
  expect(await store.get('dashboard', 'c')).toBeNull();
  await store.flush();
  const again = new CacheStore(dir, safe, 'darwin');
  expect(await again.get('mirror', 'b/p')).toBe('"b"');
  expect(await again.get('mirror', 'a/p')).toBeNull();
});

test('oversized values are not kept, and the oldest entries go first past the limits', async () => {
  const { store } = setup();
  await store.set('mirror', 'h', 'h/big', '"small"');
  await store.set('mirror', 'h', 'h/big', 'x'.repeat(MAX_ENTRY + 1));
  expect(await store.get('mirror', 'h/big')).toBeNull();
  let t = 0;
  const timed = new CacheStore(mkdtempSync(join(tmpdir(), 'vk-cache-')), setup().safe, 'darwin', 10, () => ++t);
  for (let i = 0; i < 300; i++) await timed.set('mirror', 'h', `h/${i}`, '"x"');
  expect(await timed.get('mirror', 'h/0')).toBeNull();
  expect(await timed.get('mirror', 'h/299')).toBe('"x"');
  await timed.flush();
});

test('a broken file is dropped instead of failing the app', async () => {
  const { dir, safe } = setup();
  writeFileSync(join(dir, 'cache.bin'), 'not encrypted json');
  const store = new CacheStore(dir, safe, 'darwin', 10);
  expect(await store.get('dashboard', 'h')).toBeNull();
  await store.set('dashboard', 'h', 'h', '"ok"');
  expect(await store.get('dashboard', 'h')).toBe('"ok"');
});

test('a write in progress still counts as pending', async () => {
  const { store } = setup();
  await store.set('dashboard', 'h', 'h', '"x"');
  const writing = store.flush();
  await Promise.resolve();
  await Promise.resolve();
  expect(store.hasPending()).toBe(true);
  await writing;
  expect(store.hasPending()).toBe(false);
});
