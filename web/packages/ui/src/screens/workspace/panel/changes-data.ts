// The file list behind the Changes tab and the centre diff: the working tree (`git.status`,
// polled) or a base/commit source (`git.diff {base|range}` without a file).

import { useEffect, useMemo, useState } from 'react';
import type { GitFile, GitRevFile, GitStatus } from '@vibeke/core';
import { useApp } from '../../../app/hooks';
import { errorMessage } from '../../../lib/answer';
import { statusLetter } from '../../../lib/changes';
import { useGitStatus } from '../../../lib/use-git-status';
import type { DiffFile } from './diff-pane';
import { listParams, type DiffSource } from './routes';

export interface ChangeFiles {
  status: GitStatus | null;
  files: DiffFile[];
  truncated: boolean;
  loading: boolean;
  error: string | null;
  /** Changes whenever the working tree's file list or counts change. */
  signature: string;
  refresh(): void;
}

export const workFile = (f: GitFile): DiffFile => ({ path: f.path, adds: f.adds, dels: f.dels, letter: statusLetter(f), binary: f.binary, secret: f.secret, orig_path: f.orig_path });
export const revFile = (f: GitRevFile): DiffFile => ({ path: f.path, adds: f.adds, dels: f.dels, letter: null, binary: f.binary, secret: f.secret });

export function useChangeFiles(host: string, pane: string | null, src: DiffSource, o: { poll?: boolean } = {}): ChangeFiles {
  const app = useApp();
  const gs = useGitStatus(host, pane, { poll: o.poll });
  const signature = useMemo(() => (gs.status ? gs.status.files.map((f) => `${f.path}:${f.kind}:${f.adds ?? ''}:${f.dels ?? ''}`).join('|') : ''), [gs.status]);
  const [rev, setRev] = useState<{ key: string; files: GitRevFile[]; truncated: boolean; error: string | null } | null>(null);
  const [nonce, setNonce] = useState(0);
  const params = pane ? listParams(pane, src) : null;
  const key = params ? JSON.stringify(params) : '';

  useEffect(() => {
    if (!params) return;
    let live = true;
    app
      .conn(host)
      ?.request('git.diff', params)
      .then(
        (r) => live && setRev({ key, files: r.files ?? [], truncated: !!r.truncated, error: null }),
        (e) => live && setRev({ key, files: [], truncated: false, error: errorMessage(e) }),
      );
    return () => {
      live = false;
    };
    // A base compares with the working tree: follow its changes. A commit never changes.
  }, [app, host, key, src.kind === 'base' ? signature : null, nonce]);

  const work = src.kind === 'work';
  const files = useMemo(() => (work ? (gs.status?.files ?? []).map(workFile) : rev?.key === key ? rev.files.map(revFile) : []), [work, gs.status, rev, key]);
  return {
    status: gs.status,
    files,
    truncated: work ? !!gs.status?.truncated : !!rev?.truncated,
    loading: work ? gs.loading : rev?.key !== key,
    error: work ? (gs.status ? null : gs.error) : rev?.key === key ? rev.error : null,
    signature,
    refresh: () => {
      gs.refresh();
      setNonce((n) => n + 1);
    },
  };
}
