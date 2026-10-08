import type { UpdateState } from '@vibeke/ui';
import { newer, type DesktopRelease } from './update-release';

export interface Installer {
  download(release: DesktopRelease, progress: (percent: number) => void): Promise<void>;
  install(onError: (error: Error) => void): void;
}
export interface UpdateDeps {
  version: string;
  manualReason: string | null;
  discover(): Promise<DesktopRelease>;
  installer(): Installer;
  changed(state: UpdateState): void;
  beforeInstall(): void;
  installFailed?(): void;
}

/** One controller in main serves every window. Checks coalesce; downloads and installs are
 * explicit. A failed download can be retried against the same authenticated release. */
export class UpdateController {
  private state: UpdateState;
  private release: DesktopRelease | null = null;
  private checking: Promise<void> | null = null;
  private downloading = false;
  private installing = false;
  private native: Installer | null = null;
  constructor(private readonly d: UpdateDeps) { this.state = { status: 'idle', currentVersion: d.version }; }
  snapshot(): UpdateState { return { ...this.state }; }
  private set(patch: Partial<UpdateState>): void { this.state = { ...this.state, ...patch, revision: (this.state.revision ?? 0) + 1 }; this.d.changed(this.snapshot()); }
  check(): Promise<void> {
    if (this.checking) return this.checking;
    if (this.downloading || this.state.status === 'ready' || this.installing) return Promise.resolve();
    this.checking = this.doCheck().finally(() => { this.checking = null; });
    return this.checking;
  }
  private async doCheck(): Promise<void> {
    this.set({ status: 'checking', message: undefined });
    try {
      const r = await this.d.discover();
      this.release = r;
      this.set({ status: newer(r.version, this.d.version) ? 'available' : 'up-to-date', version: r.version,
        releaseUrl: r.releaseUrl, downloadUrl: r.downloadUrl,
        manualReason: this.d.manualReason ?? (r.channel ? undefined : 'This release requires a manual installation.') });
    } catch (e) { this.release = null; this.set({ status: 'error', message: (e as Error).message, version: undefined, releaseUrl: undefined, downloadUrl: undefined }); }
  }
  async download(): Promise<void> {
    if (this.downloading || this.checking || this.state.status === 'ready') return;
    if (!this.release || this.state.manualReason || !['available', 'error'].includes(this.state.status)) throw new Error('Check for an installable update first');
    this.downloading = true;
    this.set({ status: 'downloading', progress: 0, message: undefined });
    try {
      this.native ??= this.d.installer();
      await this.native.download(this.release, (percent) => this.set({ progress: Math.max(0, Math.min(100, Math.round(percent))) }));
      this.set({ status: 'ready', progress: 100 });
    } catch (e) { this.set({ status: 'error', message: (e as Error).message }); }
    finally { this.downloading = false; }
  }
  install(): void {
    if (this.installing) return;
    if (this.state.status !== 'ready' || !this.native) throw new Error('Download and verify the update first');
    this.installing = true;
    try { this.d.beforeInstall(); this.native.install((e) => this.failedInstall(e)); }
    catch (e) { this.failedInstall(e as Error); throw e; }
  }
  private failedInstall(e: Error): void {
    this.d.installFailed?.();
    this.installing = false;
    this.set({ status: 'error', message: e.message });
  }
}
