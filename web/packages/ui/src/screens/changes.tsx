// Changes (spec 16 §9.1): one pane's read-only git status, filter by path/status, diff with
// light syntax highlighting, prev/next file; status polling lives in lib/use-git-status.ts.

import { useEffect, useState } from 'react';
import { ArrowLeft, ChevronLeft, ChevronRight, EyeOff, FileDiff, GitBranch, RefreshCw } from 'lucide-react';
import type { GitDiff, GitFile, GitStatus } from '@vibeke/core';
import { useApp, useTree } from '../app/hooks';
import { DiffView } from '../components/diff';
import { FileIcon } from '../components/file-icon';
import { DiffCount, Empty, IconButton, Notice, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { filterFiles, splitPath, statusLetter, type ChangeFilter } from '../lib/changes';
import { basename } from '../lib/format';
import { usePrefs } from '../app/hooks';
import { useGitStatus } from '../lib/use-git-status';

interface RepoState {
  host: string;
  pane: string;
  status: GitStatus | null;
}

/** One pane's working-tree changes as a filterable list (pane screen); polling in useGitStatus. */
export function ChangesPanel({ only }: { only?: { host: string; pane: string } }) {
  const tree = useTree();
  const prefs = usePrefs();
  const gs = useGitStatus(only?.host ?? null, only?.pane ?? null);
  const [filter, setFilter] = useState<ChangeFilter>('all');
  const [query, setQuery] = useState('');
  const [open, setOpen] = useState<string | null>(null);
  const st = gs.status;
  const repo: RepoState | null = only ? { host: only.host, pane: only.pane, status: st } : null;

  if (open && repo) {
    const files = st ? filterFiles(st.files, filter, query) : [];
    return <DiffScreen repo={repo} files={files} path={open} onPath={setOpen} onBack={() => setOpen(null)} fontSize={prefs.termFont} />;
  }

  return (
    <div className="min-h-0 flex-1 overflow-y-auto pb-4">
      <div className="sticky top-0 z-10 space-y-2 border-b border-border bg-bg px-3 py-2.5">
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={t.changes.filter}
          data-find-input
          aria-keyshortcuts="/"
          className="h-7 w-full rounded-md border border-border bg-surface px-2.5 text-sm placeholder:text-faint focus:border-border-strong focus:outline-none"
        />
        <div className="flex gap-1.5 overflow-x-auto no-scrollbar">
          {(['all', 'staged', 'unstaged', 'untracked'] as ChangeFilter[]).map((f) => (
            <button
              key={f}
              type="button"
              onClick={() => setFilter(f)}
              aria-pressed={filter === f}
              className={cx('vk-focus h-6 shrink-0 rounded-full border px-2.5 text-xs', filter === f ? 'border-border-strong bg-selected text-fg' : 'border-border text-muted hover:text-fg')}
            >
              {t.changes[f]}
            </button>
          ))}
        </div>
      </div>
      {!only && <Empty icon={<FileDiff className="size-10" />} title={t.changes.noPane} />}
      {only && gs.loading && (
        <div className="flex justify-center py-10">
          <Spinner />
        </div>
      )}
      {only && !gs.loading && !st && <Notice tone="warn" className="m-3">{gs.error ?? t.changes.notRepo}</Notice>}
      {st && (
        <section className="mt-2.5">
          <div className="flex items-center gap-2 px-3 pb-1 text-xs">
            <span className="font-semibold">{basename(st.repo_root)}</span>
            {st.branch && (
              <span className="flex items-center gap-1 text-muted">
                <GitBranch className="size-3.5" />
                {st.branch}
              </span>
            )}
            {st.ahead > 0 && <span className="text-muted">{t.changes.ahead(st.ahead)}</span>}
            {st.behind > 0 && <span className="text-muted">{t.changes.behind(st.behind)}</span>}
            {tree.hosts.length > 1 && <span className="text-faint">· {tree.hosts.find((h) => h.host.record.host_id === only?.host)?.host.record.name}</span>}
          </div>
          {st.clean ? (
            <div className="px-3 text-sm text-muted">{t.changes.clean}</div>
          ) : (
            <div className="px-1.5">
              {filterFiles(st.files, filter, query).map((f) => (
                <FileRow key={f.path} f={f} onClick={() => setOpen(f.path)} />
              ))}
            </div>
          )}
          {st.truncated && <div className="px-4 pt-1 text-xs text-faint">{t.changes.truncatedList}</div>}
        </section>
      )}
    </div>
  );
}

function FileRow({ f, onClick }: { f: GitFile; onClick(): void }) {
  const { dir, name } = splitPath(f.path);
  const letter = statusLetter(f);
  const tone = { A: 'text-ok', '?': 'text-ok', D: 'text-danger', U: 'text-warn', R: 'text-accent', M: 'text-warn' }[letter] ?? 'text-muted';
  return (
    <button type="button" onClick={onClick} className="vk-focus flex min-h-[var(--row-h)] w-full items-center gap-2 rounded-md px-1.5 text-left hover:bg-hover pointer-coarse:min-h-9">
      <FileIcon path={f.path} />
      <span className="min-w-0 flex-1 truncate text-sm">
        <span className="text-faint">{dir}</span>
        {name}
      </span>
      {f.secret && <EyeOff className="size-3.5 text-faint" />}
      {f.binary ? <span className="text-2xs text-faint">bin</span> : <DiffCount adds={f.adds ?? 0} dels={f.dels ?? 0} />}
      <span className={cx('w-3 text-center font-mono text-2xs font-semibold', tone)}>{letter}</span>
    </button>
  );
}

function DiffScreen({
  repo,
  files,
  path,
  onPath,
  onBack,
  fontSize,
}: {
  repo: RepoState | null;
  files: GitFile[];
  path: string;
  onPath(p: string): void;
  onBack(): void;
  fontSize: number;
}) {
  const app = useApp();
  const [diff, setDiff] = useState<GitDiff | null>(null);
  const [error, setError] = useState<string | null>(null);
  const i = files.findIndex((f) => f.path === path);
  const file = files[i];

  const load = () => {
    if (!repo) return;
    setDiff(null);
    setError(null);
    app
      .conn(repo.host)
      ?.request('git.diff', { pane: repo.pane, file: path })
      .then(setDiff, (e) => setError(errorMessage(e)));
  };
  useEffect(load, [path, repo?.pane]);

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex items-center gap-1 border-b border-border px-1 py-1">
        <IconButton label={t.back} onClick={onBack}>
          <ArrowLeft className="size-5" />
        </IconButton>
        <div className="min-w-0 flex-1 truncate font-mono text-sm">{path}</div>
        <IconButton label={t.refresh} onClick={load}>
          <RefreshCw className="size-4.5" />
        </IconButton>
        <IconButton label={t.changes.prevFile} disabled={i <= 0} onClick={() => i > 0 && onPath(files[i - 1]!.path)}>
          <ChevronLeft className="size-5" />
        </IconButton>
        <IconButton label={t.changes.nextFile} disabled={i < 0 || i >= files.length - 1} onClick={() => onPath(files[i + 1]!.path)}>
          <ChevronRight className="size-5" />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 overflow-auto p-2">
        {error && <Notice tone="danger">{error}</Notice>}
        {!diff && !error && (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        )}
        {diff && (diff.secret || file?.secret) && <Notice>{t.changes.secret}</Notice>}
        {diff && !diff.secret && diff.binary && <Notice>{t.changes.binary}</Notice>}
        {diff && !diff.secret && !diff.binary && (
          <>
            {diff.truncated && <Notice tone="warn" className="mb-2">{t.changes.truncated}</Notice>}
            {diff.diff ? <DiffView diff={diff.diff} path={path} fontSize={fontSize} /> : <div className="p-4 text-sm text-muted">{t.changes.clean}</div>}
          </>
        )}
      </div>
    </div>
  );
}
