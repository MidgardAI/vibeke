// Changes (spec 16 §9.1): read-only git status grouped by repo, filter by path/status, diff with
// light syntax highlighting, prev/next file, refresh every 5 s while visible.

import { useEffect, useMemo, useRef, useState } from 'react';
import { ArrowLeft, ChevronLeft, ChevronRight, EyeOff, FileDiff, GitBranch, RefreshCw } from 'lucide-react';
import type { GitDiff, GitFile, GitStatus } from '@vibeke/core';
import { useApp, useTree, useVisible } from '../app/hooks';
import { DiffView } from '../components/diff';
import { Empty, IconButton, Notice, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { filterFiles, repoTargets, splitPath, statusLetter, type ChangeFilter } from '../lib/changes';
import { basename } from '../lib/format';
import { usePrefs } from '../app/hooks';

interface RepoState {
  host: string;
  pane: string;
  label: string;
  status: GitStatus | null;
  error: string | null;
}

const REFRESH_MS = 5000;

export function ChangesPanel({ only }: { only?: { host: string; pane: string } }) {
  const app = useApp();
  const tree = useTree();
  const visible = useVisible();
  const prefs = usePrefs();
  const [repos, setRepos] = useState<Map<string, RepoState>>(new Map());
  const [filter, setFilter] = useState<ChangeFilter>('all');
  const [query, setQuery] = useState('');
  const [open, setOpen] = useState<{ repo: string; path: string } | null>(null);
  const [loading, setLoading] = useState(true);

  const targets = useMemo(() => {
    const rows = only ? tree.all.filter((r) => r.host === only.host && r.pane.id === only.pane) : tree.all.filter((r) => r.host && r.pane);
    return repoTargets(
      rows
        .filter((r) => tree.hosts.find((h) => h.host.record.host_id === r.host)?.host.status === 'online')
        .map((r) => ({ host: r.host, pane: r.pane.id, cwd: r.run?.cwd ?? r.pane.cwd, label: r.workspace ? r.workspace.name ?? r.workspace.auto_name : '' })),
    );
  }, [tree, only?.host, only?.pane]);
  const targetsKey = targets.map((x) => x.repoKey).join('|');
  const targetsRef = useRef(targets);
  targetsRef.current = targets;

  useEffect(() => {
    if (!visible) return;
    let live = true;
    const run = async () => {
      const next = new Map<string, RepoState>();
      await Promise.all(
        targetsRef.current.map(async (tg) => {
          const conn = app.conn(tg.host);
          if (!conn) return;
          try {
            const st = await conn.request('git.status', { pane: tg.pane });
            const key = `${tg.host}\u0000${st.repo_root}`;
            if (!next.has(key)) next.set(key, { host: tg.host, pane: tg.pane, label: tg.label, status: st, error: null });
          } catch (e) {
            const msg = errorMessage(e);
            if (only) next.set(tg.repoKey, { host: tg.host, pane: tg.pane, label: tg.label, status: null, error: msg });
          }
        }),
      );
      if (live) {
        setRepos(next);
        setLoading(false);
      }
    };
    void run();
    const id = setInterval(() => void run(), REFRESH_MS);
    return () => {
      live = false;
      clearInterval(id);
    };
  }, [targetsKey, visible]);

  const list = [...repos.entries()].sort((a, b) => (a[1].status?.repo_root ?? '').localeCompare(b[1].status?.repo_root ?? ''));

  if (open) {
    const repo = repos.get(open.repo);
    const files = repo?.status ? filterFiles(repo.status.files, filter, query) : [];
    return <DiffScreen repo={repo ?? null} files={files} path={open.path} onPath={(p) => setOpen({ ...open, path: p })} onBack={() => setOpen(null)} fontSize={prefs.termFont} />;
  }

  return (
    <div className="min-h-0 flex-1 overflow-y-auto pb-4">
      <div className="sticky top-0 z-10 space-y-2 border-b border-border bg-bg px-3 py-2">
        <input
          value={query}
          onChange={(e) => setQuery(e.target.value)}
          placeholder={t.changes.filter}
          data-find-input
          aria-keyshortcuts="/"
          className="h-9 w-full rounded-xl border border-border bg-surface px-3 text-[14px] placeholder:text-faint"
        />
        <div className="flex gap-1.5 overflow-x-auto no-scrollbar">
          {(['all', 'staged', 'unstaged', 'untracked'] as ChangeFilter[]).map((f) => (
            <button
              key={f}
              type="button"
              onClick={() => setFilter(f)}
              className={cx('h-7 shrink-0 rounded-full border px-3 text-[12px]', filter === f ? 'border-accent bg-accent/10' : 'border-border text-muted')}
            >
              {t.changes[f]}
            </button>
          ))}
        </div>
      </div>
      {loading && (
        <div className="flex justify-center py-10">
          <Spinner />
        </div>
      )}
      {!loading && list.length === 0 && <Empty icon={<FileDiff className="size-10" />} title={targets.length ? t.changes.clean : t.changes.noPane} />}
      {list.map(([key, r]) => {
        if (!r.status) return <Notice key={key} tone="warn" className="m-3">{r.error ?? t.changes.notRepo}</Notice>;
        const st = r.status;
        const files = filterFiles(st.files, filter, query);
        return (
          <section key={key} className="mt-3">
            <div className="flex items-center gap-2 px-4 pb-1.5 text-[13px]">
              <span className="font-semibold">{basename(st.repo_root)}</span>
              {st.branch && (
                <span className="flex items-center gap-1 text-muted">
                  <GitBranch className="size-3.5" />
                  {st.branch}
                </span>
              )}
              {st.ahead > 0 && <span className="text-muted">{t.changes.ahead(st.ahead)}</span>}
              {st.behind > 0 && <span className="text-muted">{t.changes.behind(st.behind)}</span>}
              {tree.hosts.length > 1 && <span className="text-faint">· {tree.hosts.find((h) => h.host.record.host_id === r.host)?.host.record.name}</span>}
            </div>
            {st.clean ? (
              <div className="px-4 text-[13px] text-muted">{t.changes.clean}</div>
            ) : (
              <div className="inset-group divide-y divide-border border-y border-border bg-surface">
                {files.map((f) => (
                  <FileRow key={f.path} f={f} onClick={() => setOpen({ repo: key, path: f.path })} />
                ))}
              </div>
            )}
            {st.truncated && <div className="px-4 pt-1 text-[12px] text-faint">{t.changes.truncatedList}</div>}
          </section>
        );
      })}
    </div>
  );
}

function FileRow({ f, onClick }: { f: GitFile; onClick(): void }) {
  const { dir, name } = splitPath(f.path);
  const letter = statusLetter(f);
  const tone = { A: 'text-ok', '?': 'text-ok', D: 'text-danger', U: 'text-warn', R: 'text-accent', M: 'text-warn' }[letter] ?? 'text-muted';
  return (
    <button type="button" onClick={onClick} className="flex w-full items-center gap-3 px-4 py-2 text-left active:bg-surface-2">
      <span className={cx('w-4 font-mono text-[13px] font-semibold', tone)}>{letter}</span>
      <span className="min-w-0 flex-1 truncate font-mono text-[13px]">
        <span className="text-faint">{dir}</span>
        {name}
      </span>
      {f.secret && <EyeOff className="size-3.5 text-faint" />}
      {f.binary ? (
        <span className="text-[11px] text-faint">bin</span>
      ) : (
        <span className="shrink-0 font-mono text-[11px]">
          {f.adds != null && <span className="text-ok">+{f.adds}</span>} {f.dels != null && <span className="text-danger">-{f.dels}</span>}
        </span>
      )}
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
        <div className="min-w-0 flex-1 truncate font-mono text-[13px]">{path}</div>
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

export function ChangesScreen() {
  return (
    <div className="flex h-full min-h-0 flex-col">
      <ChangesPanel />
    </div>
  );
}
