// Parsing the packaged update configuration (Electron-free, unit tested).

/** The feed URL in an app-update.yml (generic provider), or null when absent/invalid. */
export function packagedFeed(yml: string): string | null {
  const provider = /^provider:\s*['"]?([a-z0-9]+)['"]?\s*$/m.exec(yml)?.[1];
  const url = /^url:\s*['"]?(\S+?)['"]?\s*$/m.exec(yml)?.[1];
  if (provider !== 'generic' || !url) return null;
  try {
    const u = new URL(url);
    return u.protocol === 'https:' && !u.username && !u.password ? u.toString() : null;
  } catch {
    return null;
  }
}
