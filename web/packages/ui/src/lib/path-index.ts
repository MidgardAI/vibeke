// Which candidate paths exist as files in the workspace, from `fs.list` of their parent
// directories: one request per distinct directory, answers cached, so scanning a screenful of
// output costs a handful of calls. Only files that were listed count as existing; a truncated
// listing that lacks the name stays "unknown" (no link).

import type { FsList } from '@vibeke/core';

export const PATH_INDEX_TTL_MS = 60_000;
/** Most directories listed per `ensure` call. */
export const MAX_DIRS_PER_ENSURE = 12;

export const parentDir = (path: string): string => (path.includes('/') ? path.slice(0, path.lastIndexOf('/')) : '');
const baseName = (path: string): string => path.slice(path.lastIndexOf('/') + 1);

interface Dir {
  at: number;
  files: Set<string>;
  truncated: boolean;
}

export class PathIndex {
  private dirs = new Map<string, Dir>();
  private inflight = new Map<string, Promise<void>>();

  constructor(
    private list: (dir: string) => Promise<FsList>,
    private now: () => number = Date.now,
    private ttl = PATH_INDEX_TTL_MS,
  ) {}

  private fresh(dir: string): Dir | undefined {
    const d = this.dirs.get(dir);
    return d && this.now() - d.at < this.ttl ? d : undefined;
  }

  /**
   * Known state of `path`: true (a file), false (known missing), undefined (not looked up).
   * An old answer still counts until `ensure` replaces it, so links do not blink off.
   */
  has(path: string): boolean | undefined {
    const d = this.dirs.get(parentDir(path));
    if (!d) return undefined;
    if (d.files.has(baseName(path))) return true;
    return d.truncated ? undefined : false;
  }

  /** List the directories of `paths` that are not cached yet. Resolves with the number of requests made. */
  async ensure(paths: string[]): Promise<number> {
    const need: string[] = [];
    for (const p of paths) {
      const dir = parentDir(p);
      if (!this.fresh(dir) && !need.includes(dir)) need.push(dir);
    }
    const todo = need.slice(0, MAX_DIRS_PER_ENSURE);
    await Promise.all(todo.map((dir) => this.load(dir)));
    return todo.length;
  }

  private load(dir: string): Promise<void> {
    const running = this.inflight.get(dir);
    if (running) return running;
    const p = this.list(dir)
      .then(
        (l) => {
          this.dirs.set(dir, { at: this.now(), files: new Set(l.entries.filter((e) => e.kind === 'file' && !e.secret).map((e) => e.name)), truncated: !!l.truncated });
        },
        () => {
          // A directory that cannot be listed has no linkable files; remember that for a while.
          this.dirs.set(dir, { at: this.now(), files: new Set(), truncated: false });
        },
      )
      .finally(() => this.inflight.delete(dir));
    this.inflight.set(dir, p);
    return p;
  }

  clear(): void {
    this.dirs.clear();
  }
}

/** The first candidate that exists, or null. */
export function firstExisting(index: Pick<PathIndex, 'has'>, candidates: string[]): string | null {
  for (const c of candidates) if (index.has(c)) return c;
  return null;
}
