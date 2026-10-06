// Pure route helpers for the right panel: which diff a workspace route asks for, and where a
// click on a file, a commit or the compare menu goes.

import type { GitDiffParams, GitStatus } from '@vibeke/core';
import type { WorkspaceRoute } from '../../../router';

/** git's empty tree: the parent of a root commit. */
export const EMPTY_TREE = '4b825dc642cb6eb9a060e54bf8d69288fbee4904';

/** What the Changes tab compares. */
export type DiffSource = { kind: 'work' } | { kind: 'base'; base: string } | { kind: 'commit'; sha: string; range: string };

/** `sha^..sha`, or the empty tree for a root commit (it has no parent). */
export function commitRange(sha: string, root = false): string {
  return root ? `${EMPTY_TREE}..${sha}` : `${sha}^..${sha}`;
}

export function diffSource(route: Pick<WorkspaceRoute, 'commit' | 'base'>, rootCommit = false): DiffSource {
  if (route.commit) return { kind: 'commit', sha: route.commit, range: commitRange(route.commit, rootCommit) };
  if (route.base) return { kind: 'base', base: route.base };
  return { kind: 'work' };
}

/** `git.diff` params for one file of a source. */
export function fileDiffParams(pane: string, src: DiffSource, file: string, staged?: boolean): GitDiffParams {
  switch (src.kind) {
    case 'commit':
      return { pane, range: src.range, file };
    case 'base':
      return { pane, base: src.base, file };
    default:
      return staged ? { pane, file, staged: true } : { pane, file };
  }
}

/** `git.diff` params listing a base/commit source's files (null for the working tree). */
export function listParams(pane: string, src: DiffSource): GitDiffParams | null {
  if (src.kind === 'commit') return { pane, range: src.range };
  if (src.kind === 'base') return { pane, base: src.base };
  return null;
}

/** Clicking a file: the inline diff in the panel, or (⌥-click) the centre's transient view. */
export function fileRoute(route: WorkspaceRoute, file: string, o: { centre?: boolean } = {}): WorkspaceRoute {
  if (o.centre) return { ...route, file, view: 'diff' };
  return { ...route, file, view: null };
}

/** Clicking a commit shows its files; clicking it again (or back) returns to the changes. */
export function commitRoute(route: WorkspaceRoute, sha: string | null): WorkspaceRoute {
  return { ...route, commit: sha, file: null, base: null, view: null };
}

/** The compare menu: uncommitted (null) or a base ref. */
export function baseRoute(route: WorkspaceRoute, base: string | null): WorkspaceRoute {
  return { ...route, base, commit: null, file: null, view: null };
}

/** Back from an inline diff: the list it came from. */
export function closeFileRoute(route: WorkspaceRoute): WorkspaceRoute {
  return { ...route, file: null, view: null };
}

/** Refs worth comparing with: the task's base, then the upstream; never the branch itself. */
export function baseCandidates(status: Pick<GitStatus, 'branch' | 'upstream'> | null, task: { readonly [k: string]: unknown } | null | undefined): string[] {
  const out: string[] = [];
  const add = (r: unknown) => {
    if (typeof r === 'string' && r && r !== status?.branch && !out.includes(r)) out.push(r);
  };
  add(task?.base_ref);
  add(status?.upstream);
  return out;
}

/** True when the centre should show the panel's `CentreDiff` instead of the tab. */
export const showsCentreDiff = (route: Pick<WorkspaceRoute, 'view' | 'file'>): boolean => route.view === 'diff' && !!route.file;

// ---- root commits (shared by the panel and the centre view) ---------------------------------

const rootCommits = new Map<string, true>();
const rootListeners = new Set<() => void>();
const rootKey = (host: string, pane: string, sha: string) => `${host}\u0000${pane}\u0000${sha}`;

/** Remember that `sha` has no parent (from the log, or because `sha^` did not resolve). */
export function markRootCommit(host: string, pane: string, sha: string): void {
  const k = rootKey(host, pane, sha);
  if (rootCommits.has(k)) return;
  rootCommits.set(k, true);
  for (const l of [...rootListeners]) l();
}

export function isKnownRootCommit(host: string, pane: string, sha: string): boolean {
  return rootCommits.has(rootKey(host, pane, sha));
}

export function onRootCommits(cb: () => void): () => void {
  rootListeners.add(cb);
  return () => rootListeners.delete(cb);
}

/**
 * The comparison a route asks for, resolved: a commit known to be a root diffs against the empty
 * tree. The panel's list, its inline diff and the centre view all use this, so they agree.
 */
export function resolvedSource(host: string, pane: string | null, route: Pick<WorkspaceRoute, 'commit' | 'base'>): DiffSource {
  return diffSource(route, !!(route.commit && pane && isKnownRootCommit(host, pane, route.commit)));
}

/** A commit range that may have failed because the commit has no parent: the one to try next. */
export function rootFallback(src: DiffSource): DiffSource | null {
  if (src.kind !== 'commit' || src.range.startsWith(`${EMPTY_TREE}..`)) return null;
  return { kind: 'commit', sha: src.sha, range: commitRange(src.sha, true) };
}

// ---- refresh revisions ----------------------------------------------------------------------

/**
 * Counts "the working tree may have changed" moments: every new `git.status` answer (an edit that
 * keeps the line counts still produces a new answer) and every manual refresh. Open diffs and
 * base listings refetch when it moves.
 */
export class RefreshRevision {
  private status: unknown = undefined;
  private nonce = -1;
  value = 0;
  next(status: unknown, nonce: number): number {
    if (status !== this.status || nonce !== this.nonce) {
      if (this.nonce !== -1) this.value++;
      this.status = status;
      this.nonce = nonce;
    }
    return this.value;
  }
}

/** What makes a source's file listing stale: base comparisons follow the tree; commits only a manual refresh. */
export function listReload(src: DiffSource, revision: number, nonce: number): number | null {
  if (src.kind === 'base') return revision;
  if (src.kind === 'commit') return nonce;
  return null;
}

/** What makes an open file diff stale: the working tree for uncommitted / base; a manual refresh for a commit. */
export function diffReload(src: DiffSource, revision: number, nonce: number): number {
  return src.kind === 'commit' ? nonce : revision;
}
