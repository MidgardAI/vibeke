import { afterEach, expect, test } from 'bun:test';
import { mkdtempSync, readFileSync, rmSync, statSync } from 'node:fs';
import { join } from 'node:path';
import { tmpdir } from 'node:os';
import { DraftStore } from '../src/main/drafts';

const dirs: string[] = [];
afterEach(() => { for (const d of dirs.splice(0)) rmSync(d, { recursive: true, force: true }); });
function setup() {
  const dir = mkdtempSync(join(tmpdir(), 'vk-drafts-')); dirs.push(dir);
  const safe = { calls: 0, isEncryptionAvailable: () => true,
    encryptString(s: string) { this.calls++; return Buffer.from(Buffer.from(s).map((b) => b ^ 0x5a)); },
    decryptString(b: Buffer) { return Buffer.from(b.map((v) => v ^ 0x5a)).toString(); },
  };
  return { dir, safe, store: new DraftStore(dir, safe, 'darwin', 10) };
}
test('typing coalesces into one encrypted async write; restart flushes the last edit', async () => {
  const { dir, safe, store } = setup();
  const writes = Array.from({ length: 40 }, (_, i) => store.set('host', 'pane', `draft ${i}`));
  await Promise.all(writes);
  expect(safe.calls).toBe(1);
  expect(readFileSync(store.file).includes(Buffer.from('draft'))).toBe(false);
  expect(statSync(store.file).mode & 0o777).toBe(0o600);
  const final = store.set('host', 'pane', 'last edit');
  await store.get('host', 'pane'); // the set reached main's in-memory state
  await store.flush(); await final;
  expect(await new DraftStore(dir, safe, 'darwin').get('host', 'pane')).toBe('last edit');
  expect(store.hasPending()).toBe(false);
});
test('closed panes and unpaired hosts are removed, including late renderer writes', async () => {
  const { dir, safe, store } = setup();
  await Promise.all([store.set('a', 'p1', 'one'), store.set('a', 'p2', 'two'), store.set('b', 'p3', 'three')]);
  await store.retainPanes('a', ['p2']);
  await store.set('a', 'p1', 'stale');
  await store.retainHosts(['a']); await store.flush();
  const loaded = new DraftStore(dir, safe, 'darwin');
  expect(await loaded.get('a', 'p1')).toBe('');
  expect(await loaded.get('a', 'p2')).toBe('two');
  expect(await loaded.get('b', 'p3')).toBe('');
  await store.removeHost('a'); expect(await store.get('a', 'p2')).toBe('');
});
test('total storage is bounded and clearing a draft restores capacity', async () => {
  const { store } = setup();
  await Promise.all(Array.from({ length: 4 }, (_, i) => store.set('h', `p${i}`, 'x'.repeat(256 * 1024))));
  await expect(store.set('h', 'extra', 'x')).rejects.toThrow('full');
  await store.set('h', 'p0', ''); await store.set('h', 'extra', 'x');
});
test('failed persistence leaves a pending batch and explicit flush reports the failure', async () => {
  const { safe, store } = setup();
  safe.encryptString = () => { throw new Error('keychain locked'); };
  await expect(store.set('h', 'p', 'unsent')).rejects.toThrow('keychain locked');
  expect(store.hasPending()).toBe(true);
  await expect(store.flush()).rejects.toThrow('keychain locked');
  safe.encryptString = (s) => Buffer.from(Buffer.from(s).map((b) => b ^ 0x5a));
  await store.flush(); expect(store.hasPending()).toBe(false);
});
