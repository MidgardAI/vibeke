import { parseUpdateInfo } from 'electron-updater/out/providers/Provider';
import type { DesktopRelease } from './update-release';

export function checkedInfo(release: DesktopRelease) {
  const info = parseUpdateInfo(release.channel, release.channelName, new URL(`${release.base}/${release.channelName}`));
  if (info.version !== release.version || !Array.isArray(info.files) || !info.files.length || 'packages' in info) throw new Error('Invalid desktop update metadata');
  for (const f of info.files) {
    // Relative, versioned artifact names only: no external hosts, paths, queries or credentials.
    if (!/^[A-Za-z0-9._-]+$/.test(f.url) || !f.url.startsWith(`Vibeke-${release.version}-`) || !release.assets.includes(f.url)
        || typeof f.sha512 !== 'string' || Buffer.from(f.sha512, 'base64').length !== 64) throw new Error('Invalid update artifact');
  }
  return info;
}
