// Auto-update (spec 16 §16.2): electron-updater against the release feed that was configured at
// packaging time. electron-builder writes it to `<resources>/app-update.yml` (inside the signed
// bundle) when `VIBEKE_UPDATE_URL` is set for the build; nothing at run time (settings, renderer,
// environment) can point the updater elsewhere. Unpackaged builds and builds without a feed never
// load the updater.

import { existsSync, readFileSync } from 'node:fs';
import { join } from 'node:path';
import { app } from 'electron';
import { packagedFeed } from './updater-feed';

let started = false;

export function startUpdates(log: (msg: string) => void): void {
  if (started || !app.isPackaged) return;
  started = true;
  const file = join(process.resourcesPath, 'app-update.yml');
  if (!existsSync(file)) return log('updates: off (no feed packaged)');
  const feed = packagedFeed(readFileSync(file, 'utf8'));
  if (!feed) return log('updates: off (packaged feed must be a generic https URL)');
  // Loaded only when updates are on (keeps cold start lean): a separate bundle next to this one.
  // It reads app-update.yml itself.
  const { autoUpdater } = require(join(app.getAppPath(), 'out/main/updater-impl.cjs')) as typeof import('./updater-impl');
  autoUpdater.autoDownload = true;
  autoUpdater.autoInstallOnAppQuit = true;
  autoUpdater.on('error', (e) => log(`updates: ${e.message}`));
  const check = () => void autoUpdater.checkForUpdatesAndNotify().catch((e: Error) => log(`updates: ${e.message}`));
  check();
  setInterval(check, 6 * 60 * 60 * 1000).unref();
  log(`updates: on (${new URL(feed).host})`);
}
