// electron-updater, bundled separately (out/main/updater-impl.cjs) and required only when a
// packaged feed turns updates on: ~250 KB the main process does not parse on every start.

export { autoUpdater } from 'electron-updater';
