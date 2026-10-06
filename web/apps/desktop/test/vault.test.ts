import { afterEach, describe, expect, test } from 'bun:test';
import { mkdtempSync, readFileSync, rmSync, statSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { getOrCreateKey, loadOrCreateDeviceKey } from '@vibeke/core';
import { Vault, VaultError, checkSafeStorage, type SafeStorageLike } from '../src/main/vault';

/** A fake safeStorage: XOR "encryption" with a marker so tests can tell plaintext never lands. */
function fakeSafe(o: { available?: boolean; backend?: string } = {}): SafeStorageLike & { calls: number } {
  const k = 0x5a;
  return {
    calls: 0,
    isEncryptionAvailable: () => o.available ?? true,
    encryptString(s: string) {
      this.calls++;
      return Buffer.concat([Buffer.from('ENC1'), Buffer.from(Buffer.from(s, 'utf8').map((b) => b ^ k))]);
    },
    decryptString(b: Buffer) {
      if (b.subarray(0, 4).toString() !== 'ENC1') throw new Error('bad ciphertext');
      return Buffer.from(b.subarray(4).map((x) => x ^ k)).toString('utf8');
    },
    getSelectedStorageBackend: () => o.backend ?? 'gnome_libsecret',
  };
}

const dirs: string[] = [];
const tmp = () => {
  const d = mkdtempSync(join(tmpdir(), 'vk-vault-'));
  dirs.push(d);
  return d;
};
afterEach(() => dirs.splice(0).forEach((d) => rmSync(d, { recursive: true, force: true })));

describe('vault', () => {
  test('keys and hosts round-trip encrypted, file is 0600', async () => {
    const dir = tmp();
    const v = new Vault(dir, fakeSafe(), 'darwin');
    await v.keystore.set('k', new Uint8Array([1, 2, 3]));
    await v.hosts.put({ host_id: 'h1', relay: 'local:/tmp/gw.sock', hk: 'x', device_id: 'd', name: 'mac', scope: 'full' });
    const raw = readFileSync(v.file);
    expect(raw.subarray(0, 4).toString()).toBe('ENC1');
    expect(raw.toString('latin1')).not.toContain('gw.sock');
    expect(statSync(v.file).mode & 0o777).toBe(0o600);
    const again = new Vault(dir, fakeSafe(), 'darwin');
    expect([...((await again.keystore.get('k')) ?? [])]).toEqual([1, 2, 3]);
    expect((await again.hosts.list()).map((h) => h.host_id)).toEqual(['h1']);
    await again.hosts.remove('h1');
    await again.keystore.delete('k');
    expect(await again.hosts.list()).toEqual([]);
    expect(await again.keystore.get('k')).toBeNull();
  });

  test('getOrCreate is atomic: concurrent first starts make one key', async () => {
    const dir = tmp();
    const v = new Vault(dir, fakeSafe(), 'darwin');
    let made = 0;
    const make = () => (made++, crypto.getRandomValues(new Uint8Array(32)));
    const valid = (b: Uint8Array) => b.length === 32;
    const rs = await Promise.all([1, 2, 3, 4, 5].map(() => v.keystore.getOrCreate!('device_static', make, valid)));
    expect(made).toBe(1);
    for (const r of rs) expect([...r]).toEqual([...rs[0]!]);
    // And through core's helper with a second vault instance on the same file.
    const v2 = new Vault(dir, fakeSafe(), 'darwin');
    expect([...(await loadOrCreateDeviceKey(v2.keystore))]).toEqual([...rs[0]!]);
    expect([...(await getOrCreateKey(v2.keystore, 'device_static', make, valid))]).toEqual([...rs[0]!]);
    expect(made).toBe(1);
  });

  test('an invalid stored value is replaced', async () => {
    const v = new Vault(tmp(), fakeSafe(), 'darwin');
    await v.keystore.set('device_static', new Uint8Array(3));
    const k = await v.keystore.getOrCreate!('device_static', () => new Uint8Array(32).fill(7), (b) => b.length === 32);
    expect(k.length).toBe(32);
  });

  test('Linux basic_text backend is refused with an actionable message', async () => {
    expect(() => checkSafeStorage(fakeSafe({ backend: 'basic_text' }), 'linux')).toThrow(/keyring/);
    const v = new Vault(tmp(), fakeSafe({ backend: 'basic_text' }), 'linux');
    await expect(v.open()).rejects.toBeInstanceOf(VaultError);
    await expect(v.keystore.get('x')).rejects.toMatchObject({ code: 'basic_text' });
    // basic_text is only a Linux concept.
    expect(() => checkSafeStorage(fakeSafe({ backend: 'basic_text' }), 'darwin')).not.toThrow();
  });

  test('no encryption available → refuse, never write plaintext', async () => {
    const dir = tmp();
    const v = new Vault(dir, fakeSafe({ available: false }), 'darwin');
    await expect(v.keystore.set('k', new Uint8Array([1]))).rejects.toMatchObject({ code: 'unavailable' });
    expect(() => statSync(join(dir, 'vault.bin'))).toThrow();
  });

  test('an unreadable vault is reported, not silently replaced', async () => {
    const dir = tmp();
    writeFileSync(join(dir, 'vault.bin'), 'garbage');
    const v = new Vault(dir, fakeSafe(), 'darwin');
    await expect(v.keystore.getOrCreate!('k', () => new Uint8Array(32), () => true)).rejects.toMatchObject({ code: 'corrupt' });
    expect(readFileSync(join(dir, 'vault.bin'), 'utf8')).toBe('garbage');
  });
});
