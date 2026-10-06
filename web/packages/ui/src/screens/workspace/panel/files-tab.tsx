// Files tab: the pane's repository as a lazy tree (`fs.list` one level per folder, on demand),
// ignored entries dimmed, secrets listed but not openable; a file opens the read-only viewer
// (`?file=`, lazy-loaded). Older hosts without `fs.list` get an "update the host" note.

import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ChevronsDownUp, EyeOff, FolderTree, RefreshCw } from 'lucide-react';
import type { FsList } from '@vibeke/core';
import { useApp } from '../../../app/hooks';
import { FileIcon } from '../../../components/file-icon';
import { Empty, IconButton, Notice, Spinner } from '../../../components/ui';
import { t } from '../../../i18n';
import { errorMessage } from '../../../lib/answer';
import { sortEntries } from '../../../lib/file-tree';
import { basename } from '../../../lib/format';
import { noteUnsupported, supported } from '../../../lib/supports';
import { useGitStatus } from '../../../lib/use-git-status';
import { navigate, type WorkspaceRoute } from '../../../router';
import { toggled, usePersistedSet } from './persist';
import { TreeList, type TreeItem } from './tree';

const FileViewer = lazy(() => import('./file-viewer'));

type DirState = { list: FsList; error: null } | { list: null; error: string } | 'loading';

const join = (dir: string, name: string) => (dir ? `${dir}/${name}` : name);

export function FilesTab({ route, pane }: { route: WorkspaceRoute; pane: string }) {
  const app = useApp();
  const host = route.host;
  const [unsupported, setUnsupported] = useState(!supported(host, 'fs.list'));
  const [dirs, setDirs] = useState<Map<string, DirState>>(new Map());
  const [expanded, setExpanded] = usePersistedSet(`${host}/${route.workspace}/files`);
  const gs = useGitStatus(host, pane, { poll: false });

  const dirsRef = useRef(dirs);
  dirsRef.current = dirs;
  const gen = useRef(0);

  const load = useCallback(
    (path: string) => {
      if (dirsRef.current.has(path)) return;
      dirsRef.current = new Map(dirsRef.current).set(path, 'loading');
      setDirs(dirsRef.current);
      const g = gen.current;
      app
        .conn(host)
        ?.request('fs.list', { pane, path })
        .then(
          (list) => g === gen.current && setDirs((p) => new Map(p).set(path, { list, error: null })),
          (e) => {
            if (g !== gen.current) return;
            if (noteUnsupported(host, 'fs.list', e)) setUnsupported(true);
            else setDirs((p) => new Map(p).set(path, { list: null, error: errorMessage(e) }));
          },
        );
    },
    [app, host, pane],
  );
  const reset = useCallback(() => {
    gen.current++;
    dirsRef.current = new Map();
    setDirs(dirsRef.current);
  }, []);

  // A new pane starts over; the root and every remembered open folder load (once each).
  useEffect(reset, [host, pane]);
  useEffect(() => {
    if (unsupported) return;
    load('');
    for (const d of expanded) load(d);
  }, [load, expanded, unsupported, dirs]);

  const items = useMemo(() => {
    const out: TreeItem[] = [];
    const walk = (dir: string, depth: number, parent: string | null) => {
      const s = dirs.get(dir);
      if (!s || s === 'loading') {
        if (depth > 0) out.push({ key: `l:${dir}`, depth, dir: false, parent, name: <span className="text-faint">…</span>, inert: true });
        return;
      }
      if (!s.list) {
        out.push({ key: `e:${dir}`, depth, dir: false, parent, name: <span className="text-del">{s.error}</span>, inert: true });
        return;
      }
      if (s.list.entries.length === 0 && depth > 0) {
        out.push({ key: `z:${dir}`, depth, dir: false, parent, name: <span className="text-faint">{s.list.secret ? t.panel.secretFile : t.panel.emptyDir}</span>, inert: true });
      }
      for (const e of sortEntries(s.list.entries)) {
        const path = join(dir, e.name);
        const isDir = e.kind === 'dir';
        const open = isDir && expanded.has(path);
        const key = isDir ? `d:${path}` : `f:${path}`;
        let trailing: ReactNode = null;
        if (e.secret) trailing = <EyeOff aria-label={t.panel.secretFile} className="size-3.5 text-faint" />;
        out.push({
          key,
          depth,
          dir: isDir && !e.secret,
          open,
          parent,
          name: e.kind === 'symlink' ? <span className="italic">{e.name}</span> : e.name,
          title: e.secret ? `${path} · ${t.panel.secretFile}` : e.ignored ? `${path} · ${t.panel.ignored}` : path,
          icon: <FileIcon path={e.name} dir={isDir} />,
          trailing,
          dim: e.ignored,
          active: !isDir && route.file === path,
          inert: e.secret || e.kind === 'other',
        });
        if (open && !e.secret) walk(path, depth + 1, key);
      }
      if (s.list.truncated) out.push({ key: `t:${dir}`, depth, dir: false, parent, name: <span className="text-xs text-faint">{t.panel.truncatedDir}</span>, inert: true });
    };
    walk('', 0, null);
    return out;
  }, [dirs, expanded, route.file]);

  if (unsupported) return <Empty icon={<FolderTree />} title={t.workspace.filesUnsupported} hint={t.workspace.filesUnsupportedHint} />;

  if (route.file) {
    return (
      <Suspense
        fallback={
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        }
      >
        <FileViewer host={host} pane={pane} path={route.file} onBack={() => navigate({ ...route, file: null }, { replace: true })} />
      </Suspense>
    );
  }

  const root = dirs.get('');
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-9 shrink-0 items-center gap-1 border-b border-border pl-3 pr-1">
        <FolderTree className="size-3.5 text-faint" />
        <span className="min-w-0 flex-1 truncate text-sm text-fg/90">{gs.status ? basename(gs.status.repo_root) : t.panel.filesTree}</span>
        <IconButton label={t.panel.refresh} onClick={reset} className="size-7">
          <RefreshCw className="size-3.5" />
        </IconButton>
        <IconButton label={t.panel.collapseAll} onClick={() => setExpanded(new Set())} className="size-7">
          <ChevronsDownUp className="size-3.5" />
        </IconButton>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto">
        {(!root || root === 'loading') && (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        )}
        {root && root !== 'loading' && root.error && (
          <Notice tone="warn" className="m-3">
            {root.error}
          </Notice>
        )}
        {items.length > 0 && (
          <TreeList
            label={t.panel.filesTree}
            items={items}
            statusColumn={false}
            onToggle={(it, open) => setExpanded(toggled(expanded, it.key.slice(2), open))}
            onActivate={(it) => it.key.startsWith('f:') && navigate({ ...route, file: it.key.slice(2), view: null }, { replace: true })}
          />
        )}
      </div>
    </div>
  );
}
