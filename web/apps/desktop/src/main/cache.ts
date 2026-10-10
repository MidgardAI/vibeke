// Offline copies for the UI: each host's last dashboard and the last screen of each pane, so a
// cold start with a host offline still shows its rows and screens. Encrypted like the drafts
// (terminal output is as sensitive as drafts). It is only a cache: an unreadable or oversized file
// is dropped, failed writes are retried with the next change, and quitting never waits for it.
import { mkdir, open, readFile, rename, rm, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { checkSafeStorage, type SafeStorageLike } from './vault';

export type CacheKind = 'dashboard' | 'mirror';
type Entry = { kind: CacheKind; host: string; key: string; value: string; at: number };
export const MAX_ENTRY = 512 * 1024;
const MAX_TOTAL = 4 * 1024 * 1024;
const MAX_ENTRIES = 256;
const TTL = 30 * 24 * 60 * 60 * 1000;
const id = (kind: CacheKind, key: string) => `${kind}:${key}`;

export class CacheStore {
  readonly file: string;
  private data = new Map<string, Entry>();
  private ready: Promise<void> | undefined;
  private writing: Promise<void> = Promise.resolve();
  private dirty = false;
  private timer: ReturnType<typeof setTimeout> | undefined;
  private hosts: Set<string> | undefined;
  constructor(private dir: string, private safe: SafeStorageLike, private platform = process.platform, private delay = 10_000, private now = Date.now) {
    this.file = join(dir, 'cache.bin');
  }
  private load(): Promise<void> {
    if (this.ready) return this.ready;
    const loading = (async () => {
      await mkdir(this.dir, { recursive: true, mode: 0o700 });
      checkSafeStorage(this.safe, this.platform);
      try {
        if ((await stat(this.file)).size > MAX_TOTAL * 8) throw new Error('too large');
        const value = JSON.parse(this.safe.decryptString(await readFile(this.file)));
        if (value.v !== 1 || !Array.isArray(value.entries)) throw new Error('invalid');
        for (const e of value.entries as Entry[]) {
          if ((e.kind !== 'dashboard' && e.kind !== 'mirror') || typeof e.host !== 'string' || typeof e.key !== 'string' || typeof e.value !== 'string' || !Number.isFinite(e.at)) continue;
          this.data.set(id(e.kind, e.key), e);
        }
      } catch (e) {
        // A missing file is a first start; anything else is a broken cache that is not worth keeping.
        if ((e as NodeJS.ErrnoException).code !== 'ENOENT') {
          this.data.clear();
          await rm(this.file, { force: true }).catch(() => {});
        }
      }
      this.prune();
    })();
    this.ready = loading.catch((e) => {
      this.ready = undefined;
      throw e;
    });
    return this.ready;
  }
  /** Drops expired entries, entries of removed hosts, and the oldest ones beyond the limits. */
  private prune(): void {
    const old = this.now() - TTL;
    for (const [k, e] of this.data) if (e.at < old || (this.hosts && !this.hosts.has(e.host))) {
      this.data.delete(k);
      this.dirty = true;
    }
    let total = 0;
    for (const e of this.data.values()) total += e.value.length;
    if (total <= MAX_TOTAL && this.data.size <= MAX_ENTRIES) return;
    for (const [k, e] of [...this.data].sort((a, b) => a[1].at - b[1].at)) {
      if (total <= MAX_TOTAL && this.data.size <= MAX_ENTRIES) break;
      this.data.delete(k);
      total -= e.value.length;
      this.dirty = true;
    }
  }
  private schedule(): void {
    this.timer ??= setTimeout(() => {
      this.timer = undefined;
      void this.flush().catch(() => {});
    }, this.delay);
  }
  /** The saved JSON text, or null. */
  async get(kind: CacheKind, key: string): Promise<string | null> {
    await this.load();
    return this.data.get(id(kind, key))?.value ?? null;
  }
  /** Saves `value` (JSON text) for `host`; a value over `MAX_ENTRY` replaces nothing and is not kept. */
  async set(kind: CacheKind, host: string, key: string, value: string): Promise<void> {
    await this.load();
    if (this.hosts && !this.hosts.has(host)) return;
    if (value.length > MAX_ENTRY) {
      if (this.data.delete(id(kind, key))) this.dirty = true;
    } else {
      this.data.set(id(kind, key), { kind, host, key, value, at: this.now() });
      this.dirty = true;
      this.prune();
    }
    if (this.dirty) this.schedule();
  }
  /** Keeps only these hosts' entries (the paired hosts). */
  async retainHosts(hosts: string[]): Promise<void> {
    // Do not open the keychain only to learn the host list: apply it on the first load.
    this.hosts = new Set(hosts);
    if (!this.ready) return;
    await this.ready;
    this.prune();
    if (this.dirty) this.schedule();
  }
  async removeHost(host: string): Promise<void> {
    await this.load();
    for (const [k, e] of this.data) if (e.host === host) {
      this.data.delete(k);
      this.dirty = true;
    }
    this.hosts?.delete(host);
    await this.flush();
  }
  hasPending(): boolean {
    return this.dirty;
  }
  async flush(): Promise<void> {
    if (!this.ready) return;
    await this.ready;
    clearTimeout(this.timer);
    this.timer = undefined;
    const run = this.writing.then(async () => {
      if (!this.dirty) return;
      this.dirty = false;
      const tmp = `${this.file}.tmp`;
      try {
        checkSafeStorage(this.safe, this.platform);
        const bytes = this.safe.encryptString(JSON.stringify({ v: 1, entries: [...this.data.values()] }));
        const f = await open(tmp, 'w', 0o600);
        try {
          await f.writeFile(bytes);
        } finally {
          await f.close();
        }
        await rename(tmp, this.file);
      } catch (e) {
        this.dirty = true;
        await rm(tmp, { force: true }).catch(() => {});
        throw e;
      }
    });
    this.writing = run.catch(() => {});
    await run;
  }
}
