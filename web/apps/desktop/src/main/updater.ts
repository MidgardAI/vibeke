// One updater in main; renderers can request actions but cannot choose feeds or executables.
import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { app } from 'electron';
import type { UpdateState } from '@vibeke/ui';
import { packagedFeed } from './updater-feed';
import { UPDATE_FEED, discoverRelease, fetchBytes } from './update-release';
import { UpdateController } from './update-controller';

declare const __RELEASE_KEYS__: string[];
export interface Updates {
  snapshot(): UpdateState;
  check(): Promise<void>;
  download(): Promise<void>;
  install(): void;
  automatic(enabled: boolean): void;
  stop(): void;
}

export function startUpdates(changed: (state: UpdateState) => void, beforeInstall: () => void, installFailed: () => void): Updates {
  const file = join(process.resourcesPath, 'app-update.yml');
  const feed = existsSync(file) ? packagedFeed(readFileSync(file, 'utf8')) : null;
  const policyFile = join(app.getAppPath(), 'out/main/update-policy.json');
  let macSigned = false;
  try { macSigned = JSON.parse(readFileSync(policyFile, 'utf8')).macSigned === true; } catch { /* manual update */ }
  const manualReason = !app.isPackaged ? 'Development builds must be updated from source.'
    : feed?.replace(/\/$/, '') !== UPDATE_FEED ? 'Install the latest desktop package to enable in-app updates.'
    : process.platform === 'darwin' && !macSigned ? 'This Mac build requires a manual installation.'
    : process.platform === 'linux' && !process.env.APPIMAGE ? 'Install the new DEB with your package manager, or download an AppImage.' : null;
  const controller = new UpdateController({ version: app.getVersion(), manualReason,
    discover: () => discoverRelease(process.platform, process.arch, __RELEASE_KEYS__, fetchBytes, process.platform === 'linux' && !process.env.APPIMAGE), changed, beforeInstall, installFailed,
    installer: () => {
      const impl = require(join(app.getAppPath(), 'out/main/updater-impl.cjs')) as typeof import('./updater-impl');
      return impl.createInstaller();
    },
  });
  let first: ReturnType<typeof setTimeout> | undefined;
  let interval: ReturnType<typeof setInterval> | undefined;
  const stop = () => { clearTimeout(first); clearInterval(interval); first = undefined; interval = undefined; };
  return {
    snapshot: () => controller.snapshot(), check: () => controller.check(), download: () => controller.download(), install: () => controller.install(), stop,
    automatic(enabled) {
      stop();
      if (!enabled || !app.isPackaged) return;
      first = setTimeout(() => void controller.check(), 10_000);
      interval = setInterval(() => void controller.check(), 6 * 60 * 60 * 1000);
      first.unref(); interval.unref();
    },
  };
}
