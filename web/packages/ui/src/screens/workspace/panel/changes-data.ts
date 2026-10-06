// The file list behind the Changes tab and the centre diff: the working tree (`git.status`,
// polled) or a base/commit source (`git.diff {base|range}` without a file).

import { useEffect, useMemo, useRef, useState } from 'react';
import type { GitFile, GitRevFile, GitStatus } from '@vibeke/core';
import { useApp } from '../../../app/hooks';
import { errorMessage } from '../../../lib/answer';
import { statusLetter } from '../../../lib/changes';
import { useGitStatus } from '../../../lib/use-git-status';
import type { DiffFile } from './diff-pane';
import type { WorkspaceRoute } from '../../../router';
import { RefreshRevision, diffReload, listParams, listReload, markRootCommit, onRootCommits, resolvedSource, rootFallback, type DiffSource } from './routes';

export interface ChangeFiles {
  /** The comparison, resolved (a root commit diffs against the empty tree). */
  src: DiffSource;
  status: GitStatus | null;
  files: DiffFile[];
  truncated: boolean;
  loading: boolean;
  error: string | null;
  /**
   * Moves on every new working-tree status and every manual refresh: open diffs refetch on it
   * (an edit that keeps the line counts still shows).
   */
  revision: number;
  /** Key for open diffs of this source (see `diffReload`). */
  diffReload: number;
  refresh(): void;
}

export const workFile = (f: GitFile): DiffFile => ({ path: f.path, adds: f.adds, dels: f.dels, letter: statusLetter(f), binary: f.binary, secret: f.secret, orig_path: f.orig_path });
export const revFile = (f: GitRevFile): DiffFile => ({ path: f.path, adds: f.adds, dels: f.dels, letter: f.status ?? null, binary: f.binary, secret: f.secret, orig_path: f.orig_path ?? null });

export function useChangeFiles(host: string, pane: string | null, route: Pick<WorkspaceRoute, 'commit' | 'base'>, o: { poll?: boolean } = {}): ChangeFiles {
  const app = useApp();
  const gs = useGitStatus(host, pane, { poll: o.poll });
  const [, bumpRoots] = useState(0);
  useEffect(() => onRootCommits(() => bumpRoots((n) => n + 1)), []);
  const src = resolvedSource(host, pane, route);
  const [rev, setRev] = useState<{ key: string; files: GitRevFile[]; truncated: boolean; error: string | null } | null>(null);
  const [nonce, setNonce] = useState(0);
  const counter = useRef(new RefreshRevision());
  const revision = counter.current.next(gs.status, nonce);
  const params = pane ? listParams(pane, src) : null;
  const key = params ? JSON.stringify(params) : '';

  useEffect(() => {
    if (!params || !pane) return;
    let live = true;
    const conn = app.conn(host);
    if (!conn) return;
    conn.request('git.diff', params).then(
      (r) => live && setRev({ key, files: r.files ?? [], truncated: !!r.truncated, error: null }),
      (e) => {
        if (!live) return;
        // `sha^` does not resolve for a root commit: compare with the empty tree instead.
        const alt = rootFallback(src);
        if (alt && alt.kind === 'commit') {
          conn.request('git.diff', listParams(pane, alt)!).then(
            () => live && markRootCommit(host, pane, alt.sha),
            () => live && setRev({ key, files: [], truncated: false, error: errorMessage(e) }),
          );
          return;
        }
        setRev({ key, files: [], truncated: false, error: errorMessage(e) });
      },
    );
    return () => {
      live = false;
    };
    // A base compares with the working tree: follow its changes. A commit changes only on refresh.
  }, [app, host, key, listReload(src, revision, nonce)]);

  const work = src.kind === 'work';
  const files = useMemo(() => (work ? (gs.status?.files ?? []).map(workFile) : rev?.key === key ? rev.files.map(revFile) : []), [work, gs.status, rev, key]);
  return {
    src,
    status: gs.status,
    files,
    truncated: work ? !!gs.status?.truncated : !!rev?.truncated,
    loading: work ? gs.loading : rev?.key !== key,
    error: work ? (gs.status ? null : gs.error) : rev?.key === key ? rev.error : null,
    revision,
    diffReload: diffReload(src, revision, nonce),
    refresh: () => {
      gs.refresh();
      setNonce((n) => n + 1);
    },
  };
}
