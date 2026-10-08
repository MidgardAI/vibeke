import { basename, sep } from 'node:path';

/**
 * Whether this process is the Vibeke AppImage. `APPIMAGE` alone is not enough: an app started
 * from another AppImage (a terminal emulator, say) inherits that one's variables, and the
 * AppImage updater would overwrite the file `APPIMAGE` names. Require a Vibeke AppImage name and
 * an executable inside that image's mount (`APPDIR`).
 */
export function isVibekeAppImage(env: NodeJS.ProcessEnv, execPath: string, platform: string): boolean {
  if (platform !== 'linux') return false;
  const image = env.APPIMAGE;
  const dir = env.APPDIR;
  if (!image || !dir) return false;
  if (!/^Vibeke-.*\.AppImage$/i.test(basename(image))) return false;
  const root = dir.endsWith(sep) ? dir : dir + sep;
  return execPath.startsWith(root);
}
