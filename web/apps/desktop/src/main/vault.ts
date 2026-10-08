// Device keys and host records, encrypted with Electron `safeStorage` (OS keychain / DPAPI /
// libsecret) in one file in the user-data dir (spec 16 §3, §9.3, §16.1):
//
//   <userData>/vault.bin   mode 0600, written atomically (temp + fsync + rename)
//
// The file holds `safeStorage.encryptString(JSON)` of `{v:1, keys:{name: base64}, hosts:{id: record}}`.
// Every operation runs under one in-process lock (the app holds a single-instance lock, so this
// process is the only writer), which makes `getOrCreate` atomic. On Linux the `basic_text`
// backend (a hard-coded password: no protection) is refused with a clear message.
//
// Electron-free: `safeStorage` and the file system are injected so tests run under Bun.

import { closeSync, fsyncSync, mkdirSync, openSync, readFileSync, renameSync, rmSync, writeSync, chmodSync } from 'node:fs';
import { join } from 'node:path';
import type { HostRecord, HostStore, KeyStore } from '@vibeke/core';

/** The subset of Electron's `safeStorage` we use. */
export interface SafeStorageLike {
  isEncryptionAvailable(): boolean;
  encryptString(plain: string): Buffer;
  decryptString(encrypted: Buffer): string;
  /** Linux only: `basic_text`, `gnome_libsecret`, `kwallet*`, `unknown`. */
  getSelectedStorageBackend?(): string;
}

export class VaultError extends Error {
  constructor(
    readonly code: 'unavailable' | 'basic_text' | 'corrupt',
    message: string,
  ) {
    super(message);
    this.name = 'VaultError';
  }
}

interface VaultData {
  v: 1;
  keys: Record<string, string>;
  hosts: Record<string, HostRecord>;
}

const empty = (): VaultData => ({ v: 1, keys: {}, hosts: {} });

export const KEYRING_HELP =
  'Vibeke keeps its device key in your system keyring, but no secure keyring is available ' +
  '(Electron reports the "basic_text" backend). Install and unlock GNOME Keyring or KWallet ' +
  '(e.g. `gnome-keyring` + `libsecret`), or start Vibeke with --password-store=gnome-libsecret / kwallet5, then restart.';

/** Refuse to run without real encryption (spec 16 §16.1). */
export function checkSafeStorage(s: SafeStorageLike, platform: string): void {
  if (platform === 'linux') {
    const backend = s.getSelectedStorageBackend?.() ?? 'unknown';
    if (backend === 'basic_text') throw new VaultError('basic_text', KEYRING_HELP);
  }
  if (!s.isEncryptionAvailable()) {
    throw new VaultError('unavailable', platform === 'linux' ? KEYRING_HELP : 'The system keychain is not available, so Vibeke cannot protect its device key.');
  }
}

export class Vault {
  private data: VaultData | null = null;
  private lock: Promise<unknown> = Promise.resolve();
  readonly file: string;

  constructor(
    dir: string,
    private readonly safe: SafeStorageLike,
    private readonly platform: string = process.platform,
  ) {
    this.file = join(dir, 'vault.bin');
    mkdirSync(dir, { recursive: true, mode: 0o700 });
  }

  /** Serialize `f` with every other vault operation. */
  private exclusive<T>(f: () => T | Promise<T>): Promise<T> {
    const run = this.lock.then(f, f);
    this.lock = run.catch(() => {});
    return run;
  }

  private load(): VaultData {
    if (this.data) return this.data;
    checkSafeStorage(this.safe, this.platform);
    let raw: Buffer;
    try {
      raw = readFileSync(this.file);
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code === 'ENOENT') return (this.data = empty());
      throw e;
    }
    let parsed: unknown;
    try {
      parsed = JSON.parse(this.safe.decryptString(raw));
    } catch {
      // Never silently replace keys we cannot read (another user's keychain, a restored backup):
      // the user decides (README: delete vault.bin to start over).
      throw new VaultError('corrupt', `Cannot decrypt ${this.file}. If you moved it from another account or machine, remove it to start over (you will need to pair again).`);
    }
    const o = parsed as Partial<VaultData> | null;
    if (!o || o.v !== 1 || typeof o.keys !== 'object' || typeof o.hosts !== 'object' || !o.keys || !o.hosts) {
      throw new VaultError('corrupt', `${this.file} has an unknown format.`);
    }
    return (this.data = { v: 1, keys: { ...o.keys }, hosts: { ...o.hosts } });
  }

  private save(next: VaultData): void {
    checkSafeStorage(this.safe, this.platform);
    const enc = this.safe.encryptString(JSON.stringify(next));
    const tmp = `${this.file}.${process.pid}.tmp`;
    const fd = openSync(tmp, 'w', 0o600);
    try {
      writeSync(fd, enc);
      fsyncSync(fd);
    } finally {
      closeSync(fd);
    }
    try {
      chmodSync(tmp, 0o600);
      renameSync(tmp, this.file);
    } catch (e) {
      rmSync(tmp, { force: true });
      throw e;
    }
    this.data = next;
  }

  /** Fails fast (keyring missing, unreadable vault) before anything else starts. */
  open(): Promise<void> {
    return this.exclusive(() => void this.load());
  }

  readonly keystore: KeyStore = {
    get: (name) =>
      this.exclusive(() => {
        const v = this.load().keys[name];
        return v === undefined ? null : new Uint8Array(Buffer.from(v, 'base64'));
      }),
    set: (name, value) =>
      this.exclusive(() => {
        const d = this.load();
        this.save({ ...d, keys: { ...d.keys, [name]: Buffer.from(value).toString('base64') } });
      }),
    delete: (name) =>
      this.exclusive(() => {
        const d = this.load();
        if (!(name in d.keys)) return;
        const keys = { ...d.keys };
        delete keys[name];
        this.save({ ...d, keys });
      }),
    getOrCreate: (name, make, valid) =>
      this.exclusive(() => {
        const d = this.load();
        const cur = d.keys[name];
        if (cur !== undefined) {
          const b = new Uint8Array(Buffer.from(cur, 'base64'));
          if (valid(b)) return b;
        }
        const v = make();
        this.save({ ...d, keys: { ...d.keys, [name]: Buffer.from(v).toString('base64') } });
        return v;
      }),
  };

  readonly hosts: HostStore = {
    list: () => this.exclusive(() => Object.values(this.load().hosts)),
    put: (r) =>
      this.exclusive(() => {
        const d = this.load();
        this.save({ ...d, hosts: { ...d.hosts, [r.host_id]: r } });
      }),
    remove: (id) =>
      this.exclusive(() => {
        const d = this.load();
        if (!(id in d.hosts)) return;
        const hosts = { ...d.hosts };
        delete hosts[id];
        this.save({ ...d, hosts });
      }),
  };
}
