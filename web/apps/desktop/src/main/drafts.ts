// Conversation drafts only. Updates reach main immediately; disk writes coalesce across
// windows and use async I/O. Restart/quit flushes the pending batch before exiting.
import { mkdir, open, readFile, rename, rm, stat } from 'node:fs/promises';
import { join } from 'node:path';
import { checkSafeStorage, type SafeStorageLike } from './vault';

type Draft = { host: string; pane: string; text: string; at: number };
const MAX_TEXT = 256 * 1024;
const MAX_TOTAL = 1024 * 1024;
const MAX_DRAFTS = 64;
const TTL = 30 * 24 * 60 * 60 * 1000;
const key = (host: string, pane: string) => JSON.stringify([host, pane]);

export class DraftStore {
  readonly file: string;
  private data = new Map<string, Draft>();
  private ready: Promise<void> | undefined;
  private writing: Promise<void> = Promise.resolve();
  private dirty = false;
  private saving = false;
  private admitting = 0;
  private rejected = new Map<string, { host: string; pane: string }>();
  private timer: ReturnType<typeof setTimeout> | undefined;
  private waiters: { resolve(): void; reject(e: unknown): void }[] = [];
  private hosts: Set<string> | undefined;
  private panes = new Map<string, Set<string>>();
  constructor(private dir: string, private safe: SafeStorageLike, private platform = process.platform, private delay = 400) {
    this.file = join(dir, 'drafts.bin');
  }
  private load(): Promise<void> {
    return this.ready ??= (async () => {
      await mkdir(this.dir, { recursive: true, mode: 0o700 });
      // The unreleased vault implementation also saved terminal passwords. Do not migrate it.
      await rm(join(this.dir, 'vault.bin'), { force: true });
      checkSafeStorage(this.safe, this.platform);
      try {
        if ((await stat(this.file)).size > MAX_TOTAL * 4) throw new Error('Draft storage exceeds its size limit');
        const value = JSON.parse(this.safe.decryptString(await readFile(this.file)));
        if (value.v !== 1 || !Array.isArray(value.drafts) || value.drafts.length > MAX_DRAFTS) throw new Error('Invalid draft storage');
        let size = 0;
        for (const d of value.drafts as Draft[]) {
          if (typeof d.host !== 'string' || typeof d.pane !== 'string' || typeof d.text !== 'string' || !Number.isFinite(d.at)) throw new Error('Invalid draft');
          size += Buffer.byteLength(d.text);
          if (Buffer.byteLength(d.text) > MAX_TEXT || size > MAX_TOTAL) throw new Error('Draft storage exceeds its size limit');
          this.data.set(key(d.host, d.pane), d);
        }
      } catch (e) { if ((e as NodeJS.ErrnoException).code !== 'ENOENT') throw e; }
      this.prune();
    })();
  }
  private allowed(host: string, pane: string): boolean {
    return (!this.hosts || this.hosts.has(host)) && (!this.panes.has(host) || this.panes.get(host)!.has(pane));
  }
  private prune(): void {
    for (const [k, d] of this.data) if (d.at < Date.now() - TTL || !this.allowed(d.host, d.pane)) {
      this.data.delete(k);
      this.dirty = true;
    }
    for (const [k, d] of this.rejected) if (!this.allowed(d.host, d.pane)) this.rejected.delete(k);
    if (this.dirty) this.schedule();
  }
  private schedule(): void {
    // A fixed coalescing window also persists continuous typing; it never postpones forever.
    this.timer ??= setTimeout(() => { void this.flush().catch(() => {}); }, this.delay);
  }
  async get(host: string, pane: string): Promise<string> {
    await this.load();
    this.prune();
    return this.data.get(key(host, pane))?.text ?? '';
  }
  async set(host: string, pane: string, text: string): Promise<void> {
    const k = key(host, pane);
    this.admitting++;
    try { await this.load(); } catch (e) { this.rejected.set(k, { host, pane }); throw e; } finally { this.admitting--; }
    this.prune();
    if (!this.allowed(host, pane)) return; // a late message from an already-closed pane
    let size = Buffer.byteLength(text);
    if (size > MAX_TEXT) { this.rejected.set(k, { host, pane }); throw new Error('Draft is too large'); }
    for (const [other, d] of this.data) if (other !== k) size += Buffer.byteLength(d.text);
    if (size > MAX_TOTAL || (text && !this.data.has(k) && this.data.size >= MAX_DRAFTS)) { this.rejected.set(k, { host, pane }); throw new Error('Draft storage is full. Send or clear older drafts before restarting.'); }
    this.rejected.delete(k);
    if (text) this.data.set(k, { host, pane, text, at: Date.now() }); else this.data.delete(k);
    this.dirty = true;
    const saved = new Promise<void>((resolve, reject) => this.waiters.push({ resolve, reject }));
    this.schedule();
    return saved;
  }
  async retainHosts(hosts: string[]): Promise<void> {
    await this.load();
    this.hosts = new Set(hosts);
    this.prune();
  }
  async retainPanes(host: string, panes: string[]): Promise<void> {
    await this.load();
    this.panes.set(host, new Set(panes));
    this.prune();
  }
  async removeHost(host: string): Promise<void> {
    await this.retainPanes(host, []);
    await this.flush();
  }
  hasPending(): boolean { return this.admitting > 0 || this.rejected.size > 0 || this.dirty || this.waiters.length > 0 || this.saving; }
  async flush(): Promise<void> {
    // Do not open an unused keychain just because the app is quitting.
    if (!this.ready) return;
    await this.ready;
    clearTimeout(this.timer);
    this.timer = undefined;
    const run = this.writing.then(async () => {
      while (this.dirty) {
        const batch = this.waiters.splice(0);
        this.dirty = false;
        const tmp = `${this.file}.tmp`;
        this.saving = true;
        try {
          checkSafeStorage(this.safe, this.platform);
          const bytes = this.safe.encryptString(JSON.stringify({ v: 1, drafts: [...this.data.values()] }));
          const f = await open(tmp, 'w', 0o600);
          try { await f.writeFile(bytes); await f.sync(); } finally { await f.close(); }
          await rename(tmp, this.file);
          for (const w of batch) w.resolve();
        } catch (e) {
          this.dirty = true;
          await rm(tmp, { force: true }).catch(() => {});
          for (const w of batch) w.reject(e);
          throw e;
        } finally { this.saving = false; }
      }
    });
    this.writing = run.catch(() => {});
    await run;
    if (this.rejected.size) throw new Error('Some composer drafts could not be saved. Copy or clear them before restarting.');
  }
}
