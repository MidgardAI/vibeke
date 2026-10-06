// Changes view helpers: filter and group `git.status` files (spec 16 §9.1 Changes).

import type { GitFile } from '@vibeke/core';

export type ChangeFilter = 'all' | 'staged' | 'unstaged' | 'untracked';

export function filterFiles(files: readonly GitFile[], filter: ChangeFilter, query: string): GitFile[] {
  const q = query.trim().toLowerCase();
  return files.filter((f) => {
    if (q && !f.path.toLowerCase().includes(q)) return false;
    switch (filter) {
      case 'staged':
        return f.staged ?? (f.x !== '.' && f.x !== '?' && f.x !== ' ');
      case 'unstaged':
        return f.kind !== 'untracked' && f.y !== '.' && f.y !== ' ';
      case 'untracked':
        return f.kind === 'untracked';
      default:
        return true;
    }
  });
}

/** One-letter status badge: M A D R U ?. */
export function statusLetter(f: GitFile): string {
  switch (f.kind) {
    case 'untracked':
      return '?';
    case 'added':
      return 'A';
    case 'deleted':
      return 'D';
    case 'renamed':
      return 'R';
    case 'conflicted':
      return 'U';
    default:
      return 'M';
  }
}

/** Split a path into directory + name for two-tone rendering. */
export function splitPath(p: string): { dir: string; name: string } {
  const i = p.lastIndexOf('/');
  return i < 0 ? { dir: '', name: p } : { dir: p.slice(0, i + 1), name: p.slice(i + 1) };
}

export interface RepoTarget {
  host: string;
  pane: string;
  repoKey: string;
  label: string;
}

/**
 * One `git.status` target per distinct working directory across panes (the gateway resolves the
 * repo root itself; panes in the same repo dedupe after the first status reply).
 */
export function repoTargets(panes: { host: string; pane: string; cwd: string | null; label: string }[]): RepoTarget[] {
  const seen = new Set<string>();
  const out: RepoTarget[] = [];
  for (const p of panes) {
    if (!p.cwd) continue;
    const key = `${p.host}\u0000${p.cwd}`;
    if (seen.has(key)) continue;
    seen.add(key);
    out.push({ host: p.host, pane: p.pane, repoKey: key, label: p.label });
  }
  return out;
}
