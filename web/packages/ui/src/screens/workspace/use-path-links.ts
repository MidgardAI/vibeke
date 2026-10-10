// Links for a pane's output: URLs open in the device browser; file paths that exist in the
// workspace open the file viewer. Existence comes from `fs.list` of the paths' directories
// (lib/path-index.ts), asked once per directory and cached per pane, so a screen of output costs
// a few calls. Without `openFile` (a popped-out pane window) paths stay plain text.

import { useEffect, useMemo, useState } from 'react';
import type { LinkOps } from '../../components/link-context';
import { useApp } from '../../app/hooks';
import { extractPathRefs, pathCandidates } from '../../lib/linkify';
import { safeHref } from '../../lib/markdown';
import { MAX_DIRS_PER_ENSURE, PathIndex, firstExisting } from '../../lib/path-index';
import { noteUnsupported, supported } from '../../lib/supports';
import { useGitStatus } from '../../lib/use-git-status';

const indexes = new Map<string, PathIndex>();

export function usePathLinks(hostId: string, pane: string, cwd: string | null | undefined, text: string, openFile?: (path: string, line?: number) => void): LinkOps {
  const app = useApp();
  const gs = useGitStatus(hostId, pane, { poll: false });
  const repoRoot = gs.status?.repo_root ?? null;
  const [version, setVersion] = useState(0);
  const key = `${hostId}\u0000${pane}`;
  const index = useMemo(() => {
    let i = indexes.get(key);
    if (!i) {
      i = new PathIndex(async (dir) => {
        const conn = app.conn(hostId);
        if (!conn) throw new Error('not connected');
        try {
          return await conn.request('fs.list', { pane, path: dir });
        } catch (e) {
          noteUnsupported(hostId, 'fs.list', e);
          throw e;
        }
      });
      indexes.set(key, i);
    }
    return i;
  }, [app, hostId, pane, key]);

  const canOpen = !!openFile && !!repoRoot;
  const candidates = useMemo(() => {
    if (!canOpen) return [];
    const out = new Set<string>();
    for (const raw of extractPathRefs(text)) for (const c of pathCandidates(raw, { repoRoot, cwd })) out.add(c);
    return [...out];
  }, [text, canOpen, repoRoot, cwd]);
  const wanted = candidates.join('\u0000');

  useEffect(() => {
    if (!candidates.length || !supported(hostId, 'fs.list')) return;
    let live = true;
    const step = async () => {
      const n = await index.ensure(candidates);
      if (!live) return;
      if (n > 0) setVersion((v) => v + 1);
      // A full batch may have left directories behind: ask again.
      if (n >= MAX_DIRS_PER_ENSURE) void step();
    };
    void step();
    return () => {
      live = false;
    };
  }, [index, wanted, hostId]);

  return useMemo<LinkOps>(
    () => ({
      fileFor: (raw) => (canOpen && supported(hostId, 'fs.list') ? firstExisting(index, pathCandidates(raw, { repoRoot, cwd })) : null),
      openFile: (path, line) => openFile?.(path, line),
      openUrl: (url) => {
        const safe = safeHref(url);
        if (safe) app.platform.openExternal(safe);
      },
    }),
    [index, canOpen, repoRoot, cwd, version, openFile, app, hostId],
  );
}
