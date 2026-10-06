// One file's diff under a sticky header (back, file, +/−, status, previous/next file). Used inline
// in the Changes tab and, as the transient centre view, by the workspace screen.

import { useEffect, useRef, useState } from 'react';
import { ArrowLeft, ChevronDown, ChevronUp, X } from 'lucide-react';
import type { GitDiff } from '@vibeke/core';
import { useApp, usePrefs } from '../../../app/hooks';
import { DiffView } from '../../../components/diff';
import { FileIcon } from '../../../components/file-icon';
import { DiffCount, IconButton, Notice, Spinner, cx } from '../../../components/ui';
import { t } from '../../../i18n';
import { errorMessage } from '../../../lib/answer';
import { splitPath } from '../../../lib/changes';
import { fileDiffParams, type DiffSource } from './routes';
import { StatusSquare } from './tree';

export interface DiffFile {
  path: string;
  adds?: number | null;
  dels?: number | null;
  letter: string | null;
  binary: boolean;
  secret?: boolean;
  orig_path?: string | null;
}

export function DiffPane({
  host,
  pane,
  src,
  path,
  files,
  reload,
  onPath,
  onBack,
  onClose,
  centre = false,
}: {
  host: string;
  pane: string;
  src: DiffSource;
  path: string;
  /** Files in tree order (prev/next). */
  files: readonly DiffFile[];
  /** Changes when the source may have changed (status poll): refetch quietly. */
  reload?: unknown;
  onPath(path: string): void;
  onBack?(): void;
  onClose?(): void;
  centre?: boolean;
}) {
  const app = useApp();
  const prefs = usePrefs();
  const [diff, setDiff] = useState<GitDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  const shown = useRef<string | null>(null);
  const root = useRef<HTMLDivElement>(null);
  // Opening from the tree unmounts the clicked row: keep focus inside (Escape, keys keep working).
  useEffect(() => {
    if (!centre && (!document.activeElement || document.activeElement === document.body)) root.current?.focus({ preventScroll: true });
  }, []);
  const i = files.findIndex((f) => f.path === path);
  const file = files[i];
  const srcKey = src.kind === 'commit' ? `c:${src.range}` : src.kind === 'base' ? `b:${src.base}` : 'w';
  const key = `${srcKey}\u0000${path}`;

  useEffect(() => {
    let live = true;
    if (shown.current !== key) {
      setDiff(null);
      setError(null);
    }
    const conn = app.conn(host);
    if (!conn) return;
    conn.request('git.diff', fileDiffParams(pane, src, path)).then(
      (d) => {
        if (!live) return;
        shown.current = key;
        setDiff(d);
        setError(null);
      },
      (e) => live && setError(errorMessage(e)),
    );
    return () => {
      live = false;
    };
  }, [host, pane, key, src.kind === 'work' ? reload : null]);

  const { dir, name } = splitPath(path);
  const prev = i > 0 ? files[i - 1] : undefined;
  const next = i >= 0 ? files[i + 1] : undefined;
  const secret = !!(diff?.secret || file?.secret);

  return (
    <div ref={root} tabIndex={-1} className={cx('flex min-h-0 flex-1 flex-col outline-none', centre && 'bg-bg')}>
      <div className={cx('sticky top-0 z-10 flex h-9 shrink-0 items-center gap-1.5 border-b border-border bg-bg pr-1', onBack ? 'pl-1' : 'pl-3', centre && 'h-11')}>
        {onBack && (
          <IconButton label={t.back} onClick={onBack} className="size-7">
            <ArrowLeft className="size-4" />
          </IconButton>
        )}
        <FileIcon path={path} />
        <div className="flex min-w-0 flex-1 items-baseline gap-1.5" title={path}>
          <span className="shrink-0 truncate text-sm font-medium">{name}</span>
          {dir && <span className="min-w-0 truncate text-xs text-muted">{dir.replace(/\/$/, '')}</span>}
        </div>
        {file && !file.binary && <DiffCount adds={file.adds ?? 0} dels={file.dels ?? 0} />}
        {file?.letter && <StatusSquare letter={file.letter} />}
        <span className="ml-1 flex items-center">
          <IconButton label={t.changes.prevFile} disabled={!prev} onClick={() => prev && onPath(prev.path)} className="size-7">
            <ChevronUp className="size-4" />
          </IconButton>
          <IconButton label={t.changes.nextFile} disabled={!next} onClick={() => next && onPath(next.path)} className="size-7">
            <ChevronDown className="size-4" />
          </IconButton>
          {onClose && (
            <IconButton label={t.panel.closeDiff} onClick={onClose} className="size-7">
              <X className="size-4" />
            </IconButton>
          )}
        </span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto" data-diff-scroll>
        {file?.orig_path && <div className="border-b border-border px-3 py-1 text-xs text-muted">{t.panel.renamedFrom(file.orig_path)}</div>}
        {error && (
          <Notice tone="danger" className="m-3">
            {error}
          </Notice>
        )}
        {!diff && !error && (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        )}
        {diff && secret && <Notice className="m-3">{t.changes.secret}</Notice>}
        {diff && !secret && diff.binary && <Notice className="m-3">{t.changes.binary}</Notice>}
        {diff && !secret && !diff.binary && (
          <>
            {diff.truncated && (
              <Notice tone="warn" className="m-2">
                {t.changes.truncated}
              </Notice>
            )}
            {diff.diff ? <DiffView diff={diff.diff} path={path} fontSize={prefs.termFont} gutter="one" bare wrap={!centre && prefs.wrap} /> : <div className="p-4 text-sm text-muted">{t.panel.noChanges}</div>}
          </>
        )}
      </div>
    </div>
  );
}
