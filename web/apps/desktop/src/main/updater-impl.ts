// Lazy bundle: native installation is loaded only after the user chooses to download.
import { app } from 'electron';
import { autoUpdater } from 'electron-updater';
import { Provider, resolveFiles, type ProviderRuntimeOptions } from 'electron-updater/out/providers/Provider';
import type { Installer } from './update-controller';
import { checkedInfo } from './update-info';

// The provider gets only the exact metadata already authenticated by update-release.ts.
// It never fetches mutable latest metadata again between checking and downloading.
export function createInstaller(): Installer {
  autoUpdater.autoDownload = false;
  autoUpdater.autoInstallOnAppQuit = false;
  autoUpdater.allowPrerelease = false;
  autoUpdater.allowDowngrade = false;
  // An error is also reported by the operation's rejected promise; EventEmitter still needs
  // a listener so an update error never crashes the app.
  let installError: ((error: Error) => void) | undefined;
  autoUpdater.on('error', (error) => installError?.(error));
  return {
    async download(release, progress) {
      installError = undefined;
      if (!release.channel) throw new Error('No authenticated update metadata');
      const info = checkedInfo(release);
      class SignedProvider extends Provider<typeof info> {
        constructor(_options: unknown, _updater: unknown, runtime: ProviderRuntimeOptions) { super({ ...runtime, isUseMultipleRangeRequest: false }); }
        async getLatestVersion() { return info; }
        resolveFiles() { return resolveFiles(info, new URL(`${release.base}/`)); }
      }
      autoUpdater.setFeedURL({ provider: 'custom', updateProvider: SignedProvider });
      const onProgress = (p: { percent: number }) => progress(p.percent);
      autoUpdater.on('download-progress', onProgress);
      try {
        const result = await autoUpdater.checkForUpdates();
        if (!result || result.updateInfo.version !== release.version) throw new Error('Update version changed');
        await autoUpdater.downloadUpdate();
      } finally { autoUpdater.removeListener('download-progress', onProgress); }
    },
    install(onError) {
      const releasesLock = process.platform === 'linux' && !!process.env.APPIMAGE;
      installError = (error) => {
        if (releasesLock && !app.requestSingleInstanceLock()) app.quit();
        onError(error);
      };
      // AppImageUpdater spawns the replacement before the old process exits.
      if (releasesLock) app.releaseSingleInstanceLock();
      try { autoUpdater.quitAndInstall(true, true); } catch (error) { installError(error as Error); }
    },
  };
}
