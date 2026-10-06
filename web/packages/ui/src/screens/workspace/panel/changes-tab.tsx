// Changes tab: branch, compare mode (uncommitted / vs base) with totals, the changed files as a
// rolled-up tree, an inline diff (`?file=`), a commit's files (`?commit=`) and recent commits.

import { useEffect, useMemo, useState, type KeyboardEvent, type MouseEvent } from 'react';
import { ArrowLeft, ChevronDown, ChevronsDownUp, ChevronsUpDown, Copy, FileDiff, GitBranch, GitCommitHorizontal, MoreHorizontal, RefreshCw } from 'lucide-react';
import type { Worktree } from '@vibeke/core';
import { useApp, useHost } from '../../../app/hooks';
import { FileIcon } from '../../../components/file-icon';
import { DiffCount, Empty, IconButton, Notice, Spinner } from '../../../components/ui';
import { t } from '../../../i18n';
import { buildFileTree, dirPaths, fileOrder, totals, visibleRows } from '../../../lib/file-tree';
import { basename } from '../../../lib/format';
import { noteUnsupported, supported } from '../../../lib/supports';
import { navigate, type WorkspaceRoute } from '../../../router';
import { useChangeFiles } from './changes-data';
import { CommitsSection, isRootCommit, useGitLog } from './commits';
import { DiffPane, type DiffFile } from './diff-pane';
import { Menu, type MenuItem } from './menu';
import { toggled, usePersistedSet } from './persist';
import { baseCandidates, baseRoute, closeFileRoute, commitRoute, diffSource, fileRoute } from './routes';
import { StatusSquare, TreeList, type TreeItem } from './tree';

const go = (r: WorkspaceRoute) => navigate(r, { replace: true });

export function ChangesTab({ route, pane }: { route: WorkspaceRoute; pane: string }) {
  const app = useApp();
  const hostState = useHost(route.host);
  const host = route.host;
  const ws = hostState?.dashboard?.workspaces.find((w) => w.id === route.workspace);
  const task = ws?.task ? hostState?.dashboard?.tasks.find((k) => k.id === ws.task) : undefined;
  const turns = hostState?.dashboard?.runs.find((r) => r.pane === pane)?.turns_completed ?? 0;

  const [commitsOpen, setCommitsOpen] = usePersistedSet(`${host}/${route.workspace}/commits`);
  const showCommits = commitsOpen.has('open') || !!route.commit;
  const log = useGitLog(host, pane, turns);
  const src = diffSource(route, route.commit ? isRootCommit(log, route.commit) : false);
  const data = useChangeFiles(host, pane, src);
  const st = data.status;

  const [collapsed, setCollapsed] = usePersistedSet(`${host}/${route.workspace}/tree`);
  const tree = useMemo(() => buildFileTree(data.files), [data.files]);
  const order = useMemo(() => fileOrder(tree), [tree]);
  const ordered = useMemo(() => {
    const by = new Map(data.files.map((f) => [f.path, f]));
    return order.map((p) => by.get(p)!).filter(Boolean);
  }, [order, data.files]);
  const sum = totals(data.files);
  const bases = baseCandidates(st, task);
  const commit = route.commit ? log.commits?.find((c) => c.sha === route.commit || c.short === route.commit) : undefined;

  const items: TreeItem[] = useMemo(() => {
    const byPath = new Map<string, DiffFile>(data.files.map((f) => [f.path, f]));
    const parents: string[] = [];
    return visibleRows(tree, collapsed).map(({ node, depth, open }) => {
      parents.length = depth;
      const parent = depth > 0 ? (parents[depth - 1] ?? null) : null;
      if (node.kind === 'dir') {
        parents[depth] = `d:${node.path}`;
        return {
          key: `d:${node.path}`,
          depth,
          dir: true,
          open,
          parent,
          name: node.name,
          title: node.path,
          trailing: <DiffCount adds={node.adds} dels={node.dels} />,
        };
      }
      const f = byPath.get(node.path)!;
      return {
        key: `f:${node.path}`,
        depth,
        dir: false,
        parent,
        name: node.name,
        title: `${node.path}\n${t.panel.openHint}`,
        icon: <FileIcon path={node.path} />,
        active: route.file === node.path,
        trailing: f.binary ? <span className="text-2xs text-faint">bin</span> : <DiffCount adds={node.adds} dels={node.dels} />,
        status: f.letter ? <StatusSquare letter={f.letter} /> : null,
      };
    });
  }, [tree, collapsed, data.files, route.file]);

  const onToggle = (it: TreeItem, open?: boolean) => setCollapsed(toggled(collapsed, it.key.slice(2), open === undefined ? undefined : !open));
  const onActivate = (it: TreeItem, e: MouseEvent | KeyboardEvent) => go(fileRoute(route, it.key.slice(2), { centre: e.altKey }));

  const inlineFile = route.file && route.view !== 'diff' ? route.file : null;

  const compareItems: MenuItem[] = [
    { key: 'work', label: t.panel.uncommitted, checked: src.kind === 'work', onSelect: () => go(baseRoute(route, null)) },
    ...bases.map((b) => ({ key: `b:${b}`, label: t.panel.vsBase(b), checked: src.kind === 'base' && src.base === b, onSelect: () => go(baseRoute(route, b)) })),
    ...(bases.length ? [] : [{ key: 'none', label: t.panel.noBase, disabled: true }]),
  ];
  const moreItems: MenuItem[] = [
    { key: 'expand', label: t.panel.expandAll, icon: <ChevronsUpDown />, onSelect: () => setCollapsed(new Set()) },
    { key: 'collapse', label: t.panel.collapseAll, icon: <ChevronsDownUp />, onSelect: () => setCollapsed(new Set(dirPaths(tree))) },
    ...(st
      ? [
          {
            key: 'copy',
            label: t.panel.copyPath,
            icon: <Copy />,
            hint: basename(st.repo_root),
            onSelect: () => void navigator.clipboard?.writeText(st.repo_root).then(() => app.toast(t.panel.copied, 'ok', 1200), () => {}),
          },
        ]
      : []),
  ];

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <BranchBar host={host} pane={pane} branch={st?.branch ?? ws?.branch ?? null} ahead={st?.ahead ?? 0} behind={st?.behind ?? 0} repoRoot={st?.repo_root ?? null} />
      {route.commit ? (
        <div className="flex h-9 shrink-0 items-center gap-1 border-b border-border pl-1 pr-2">
          <IconButton label={t.panel.backToChanges} onClick={() => go(commitRoute(route, null))} className="size-7">
            <ArrowLeft className="size-4" />
          </IconButton>
          <GitCommitHorizontal className="size-3.5 shrink-0 text-faint" />
          <span className="shrink-0 font-mono text-xs text-muted">{commit?.short ?? route.commit.slice(0, 7)}</span>
          <span className="min-w-0 flex-1 truncate text-sm" title={commit?.subject}>
            {commit?.subject ?? ''}
          </span>
          <DiffCount adds={sum.adds} dels={sum.dels} />
        </div>
      ) : (
        <div className="flex h-9 shrink-0 items-center gap-1 border-b border-border pl-1.5 pr-1">
          <Menu
            label={t.panel.compare}
            items={compareItems}
            trigger={
              <>
                <span className="text-fg/90">{src.kind === 'base' ? t.panel.vsBase(src.base) : t.panel.uncommitted}</span>
                <ChevronDown className="size-3.5 text-faint" />
              </>
            }
          />
          <span data-testid="changes-total">
            <DiffCount adds={sum.adds} dels={sum.dels} />
          </span>
          <span className="flex-1" />
          <IconButton label={t.panel.refresh} onClick={data.refresh} className="size-7">
            <RefreshCw className="size-3.5" />
          </IconButton>
          <Menu label={t.panel.more} items={moreItems} align="right" triggerClassName="w-7 justify-center px-0" trigger={<MoreHorizontal className="size-4" />} />
        </div>
      )}
      {inlineFile ? (
        <DiffPane host={host} pane={pane} src={src} path={inlineFile} files={ordered} reload={data.signature} onPath={(p) => go(fileRoute(route, p))} onBack={() => go(closeFileRoute(route))} />
      ) : (
        <div className="min-h-0 flex-1 overflow-y-auto">
          {data.error && (
            <Notice tone="warn" className="m-3">
              {data.error}
            </Notice>
          )}
          {data.loading && !data.error && (
            <div className="flex justify-center py-10">
              <Spinner />
            </div>
          )}
          {!data.loading && !data.error && data.files.length === 0 && (
            <Empty icon={<FileDiff />} title={src.kind === 'base' ? t.panel.noChangesVsBase(src.base) : src.kind === 'work' ? t.changes.clean : t.panel.noChanges} />
          )}
          {items.length > 0 && <TreeList label={t.panel.tree} items={items} onToggle={onToggle} onActivate={onActivate} />}
          {data.truncated && <div className="px-3 pb-2 text-xs text-faint">{t.changes.truncatedList}</div>}
        </div>
      )}
      <CommitsSection
        log={log}
        open={showCommits}
        onToggle={() => {
          if (showCommits && route.commit) go(commitRoute(route, null));
          setCommitsOpen(toggled(commitsOpen, 'open', !showCommits));
        }}
        selected={route.commit}
        onSelect={(sha) => go(commitRoute(route, sha === route.commit ? null : sha))}
      />
    </div>
  );
}

/** `⑂ main ⌄  ↑2 ↓1`: the branch, with the repo's worktrees (read-only) in its menu. */
function BranchBar({ host, pane, branch, ahead, behind, repoRoot }: { host: string; pane: string; branch: string | null; ahead: number; behind: number; repoRoot: string | null }) {
  const app = useApp();
  const [trees, setTrees] = useState<Worktree[] | null>(null);
  useEffect(() => {
    if (!supported(host, 'worktree.list')) return;
    let live = true;
    app
      .conn(host)
      ?.request('worktree.list', { pane })
      .then(
        (r) => live && setTrees(r.worktrees ?? []),
        (e) => {
          noteUnsupported(host, 'worktree.list', e);
        },
      );
    return () => {
      live = false;
    };
  }, [app, host, pane]);
  if (!branch && !repoRoot) return null;
  const label = (
    <>
      <GitBranch className="size-3.5 text-faint" />
      <span className="max-w-[220px] truncate text-fg/90">{branch ?? 'HEAD'}</span>
    </>
  );
  const others = (trees ?? []).filter((w) => w.branch || w.head);
  return (
    <div className="flex h-9 shrink-0 items-center gap-2 pl-1.5 pr-3">
      {others.length > 1 ? (
        <Menu
          label={t.panel.branches}
          trigger={
            <>
              {label}
              <ChevronDown className="size-3.5 text-faint" />
            </>
          }
          items={others.map((w) => ({
            key: w.path,
            label: w.branch?.replace(/^refs\/heads\//, '') ?? w.head?.slice(0, 7) ?? w.path,
            hint: basename(w.path),
            checked: w.path === repoRoot,
            icon: <GitBranch />,
          }))}
        />
      ) : (
        <span className="inline-flex h-7 items-center gap-1 px-1.5 text-sm">{label}</span>
      )}
      {(ahead > 0 || behind > 0) && (
        <span className="text-xs tabular-nums text-muted">
          {ahead > 0 && t.changes.ahead(ahead)} {behind > 0 && t.changes.behind(behind)}
        </span>
      )}
    </div>
  );
}
