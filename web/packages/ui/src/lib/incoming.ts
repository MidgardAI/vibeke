// Receiving a handoff (spec 16 §15.2): the accept form's defaults, the `handoff.accept` params
// it builds, how incoming records and their events are kept, and the errors the form explains.
// Pure: screens/incoming.tsx and app/handoff-stores.ts build on it.

import type { AppApi, AppEvent, IncomingHandoff } from '@vibeke/core';

export type Suggested = AppApi['handoff.incoming.get']['result']['suggested'];
export type AcceptParams = AppApi['handoff.accept']['params'];

export type RepoMode = 'existing' | 'folder' | 'clone';

export interface AcceptForm {
  mode: RepoMode;
  /** `existing`: one of the suggested clones. */
  repo: string;
  /** `folder`: a clone the receiver points at. */
  folder: string;
  /** `clone`: where to clone the origin (its parent must exist). */
  cloneTo: string;
  worktree: string;
  branch: string;
  /** Start (resume) the agent after the import. */
  resume: boolean;
  trustMise: boolean;
  trustDirenv: boolean;
}

/** Strip a trailing slash the path picker leaves (but keep `/` and `~/` meaningful). */
export function cleanPath(p: string): string {
  const s = p.trim();
  if (s === '/' || s === '~/' || s === '~') return s === '~/' ? '~' : s;
  return s.replace(/\/+$/, '');
}

const dirOf = (p: string): string => {
  const s = cleanPath(p);
  const i = s.lastIndexOf('/');
  return i > 0 ? s.slice(0, i) : s.startsWith('/') ? '/' : '~';
};

/** The form as the receiving host suggests it (a matching clone, the remembered worktree place). */
export function initialForm(inc: IncomingHandoff, s: Suggested): AcceptForm {
  const repos = s.repos ?? [];
  const repo = s.repo ?? repos[0] ?? '';
  const name = inc.manifest.repo_name || 'repo';
  return {
    mode: repo ? 'existing' : inc.manifest.origin ? 'clone' : 'folder',
    repo,
    folder: '~/',
    cloneTo: repo ? `${dirOf(repo).replace(/\/$/, '')}/${name}-handoff` : `~/${name}`,
    worktree: s.worktree_path ?? '',
    branch: s.branch ?? inc.manifest.branch ?? '',
    resume: !!inc.manifest.harness,
    trustMise: false,
    trustDirenv: false,
  };
}

/** The suggested repos plus the one chosen by hand (if not among them), for the radio list. */
export function repoChoices(s: Suggested, chosen: string): string[] {
  const list = [...(s.repos ?? [])];
  if (s.repo && !list.includes(s.repo)) list.unshift(s.repo);
  if (chosen && !list.includes(chosen)) list.push(chosen);
  return list;
}

export type FormProblem = 'repo' | 'folder' | 'clone' | 'worktree' | 'branch';

/** A git branch name the import can create (a subset of `git check-ref-format`). */
export function validBranch(b: string): boolean {
  if (!b || b.length > 200) return false;
  if (b.startsWith('-') || b.startsWith('/') || b.endsWith('/') || b.endsWith('.') || b.endsWith('.lock')) return false;
  if (b.includes('..') || b.includes('//') || b.includes('@{') || b === '@') return false;
  // eslint-disable-next-line no-control-regex
  return !/[\s~^:?*[\\\x00-\x1f\x7f]/.test(b);
}

const absLike = (p: string): boolean => p.startsWith('/') || p === '~' || p.startsWith('~/');

function repoParam(f: AcceptForm): { repo: AcceptParams['repo'] } | { problem: FormProblem } {
  if (f.mode === 'existing') {
    const p = cleanPath(f.repo);
    return absLike(p) ? { repo: { path: p } } : { problem: 'repo' };
  }
  const p = cleanPath(f.mode === 'folder' ? f.folder : f.cloneTo);
  if (!absLike(p) || p === '~' || p === '/') return { problem: f.mode === 'folder' ? 'folder' : 'clone' };
  return { repo: f.mode === 'folder' ? { path: p } : { clone_to: p } };
}

/** `handoff.accept` params from the form, or the first field that needs fixing. */
export function acceptParams(id: string, f: AcceptForm): { ok: true; params: AcceptParams } | { ok: false; problem: FormProblem } {
  const repo = repoParam(f);
  if ('problem' in repo) return { ok: false, problem: repo.problem };
  const wt = cleanPath(f.worktree);
  if (wt && (!absLike(wt) || wt === '~' || wt === '/')) return { ok: false, problem: 'worktree' };
  const branch = f.branch.trim();
  if (!validBranch(branch)) return { ok: false, problem: 'branch' };
  const trust: ('mise' | 'direnv')[] = [];
  if (f.trustMise) trust.push('mise');
  if (f.trustDirenv) trust.push('direnv');
  return {
    ok: true,
    params: {
      id,
      repo: repo.repo,
      ...(wt ? { worktree_path: wt } : {}),
      branch,
      start_agent: f.resume,
      ...(trust.length ? { trust } : {}),
    },
  };
}

export interface RepoMismatch {
  repo: string;
  origin: string;
  remotes: string[];
}

/** `repo_mismatch` details from a failed accept (RpcError data or a record's `error`). */
export function repoMismatch(e: unknown): RepoMismatch | null {
  if (typeof e !== 'object' || e === null) return null;
  const o = e as Record<string, unknown>;
  const holder = (o.data as Record<string, unknown> | undefined) ?? o;
  const d = holder.details as Record<string, unknown> | undefined;
  if (!d || d.reason !== 'repo_mismatch') return null;
  return {
    repo: typeof d.repo === 'string' ? d.repo : '',
    origin: typeof d.origin === 'string' ? d.origin : '',
    remotes: Array.isArray(d.remotes) ? d.remotes.filter((x): x is string => typeof x === 'string') : [],
  };
}

/** Waiting for the receiver: pending, or failed (accepting again retries). */
export const needsAction = (r: IncomingHandoff): boolean => r.state === 'pending' || r.state === 'failed';

/** Count for the nav badge. */
export const actionCount = (lists: Iterable<readonly IncomingHandoff[]>): number => {
  let n = 0;
  for (const l of lists) for (const r of l) if (needsAction(r)) n++;
  return n;
};

/** Pending and failed first, then importing, then the rest; newest first within each. */
export function sortIncoming(list: readonly IncomingHandoff[]): IncomingHandoff[] {
  const rank = (r: IncomingHandoff) => (needsAction(r) ? 0 : r.state === 'importing' ? 1 : 2);
  return [...list].sort((a, b) => rank(a) - rank(b) || b.created_at_ms - a.created_at_ms);
}

const isRecord = (v: unknown): v is IncomingHandoff => {
  if (typeof v !== 'object' || v === null) return false;
  const o = v as Record<string, unknown>;
  return typeof o.id === 'string' && typeof o.state === 'string' && typeof o.manifest === 'object' && o.manifest !== null && typeof o.from === 'object' && o.from !== null;
};

export type IncomingChange =
  | { k: 'upsert'; record: IncomingHandoff; phase: string | null }
  | { k: 'remove'; id: string }
  /** The event did not carry the record: refetch the list. */
  | { k: 'refetch' }
  | null;

/** What a `handoff.incoming` / `handoff.updated` / `handoff.expired` event changes. */
export function incomingChange(e: AppEvent): IncomingChange {
  switch (e.type) {
    case 'handoff.incoming':
    case 'handoff.updated': {
      const rec = e.data.incoming;
      if (!isRecord(rec)) return { k: 'refetch' };
      const phase = typeof e.data.phase === 'string' ? e.data.phase : null;
      return { k: 'upsert', record: rec, phase };
    }
    case 'handoff.expired': {
      const id = e.subject.incoming ?? (typeof e.data.incoming === 'string' ? e.data.incoming : undefined);
      return id ? { k: 'remove', id } : { k: 'refetch' };
    }
    default:
      return null;
  }
}

/** Apply a change to one host's list (records keep their newest copy). */
export function applyIncoming(list: readonly IncomingHandoff[], c: Exclude<IncomingChange, null | { k: 'refetch' }>): IncomingHandoff[] {
  if (c.k === 'remove') return list.filter((r) => r.id !== c.id);
  const i = list.findIndex((r) => r.id === c.record.id);
  if (i < 0) return [c.record, ...list];
  if (list[i]!.updated_at_ms > c.record.updated_at_ms) return [...list];
  const next = [...list];
  next[i] = c.record;
  return next;
}

/** Where an imported handoff can be opened: its pane, else its workspace. */
export function importedTarget(r: IncomingHandoff): { pane: string } | { workspace: string } | null {
  const res = r.result;
  if (!res) return null;
  if (typeof res.pane === 'string' && res.pane) return { pane: res.pane };
  if (typeof res.workspace === 'string' && res.workspace) return { workspace: res.workspace };
  return null;
}

/** Secrets that stayed behind: the receiver brings their own (checklist on the accept view). */
export const skippedSecrets = (r: IncomingHandoff): string[] => (r.manifest.skipped ?? []).filter((s) => s.reason === 'secret').map((s) => s.path);
export const skippedOther = (r: IncomingHandoff): { path: string; reason: string }[] => (r.manifest.skipped ?? []).filter((s) => s.reason !== 'secret');

/** The destination can resume the same agent session. */
export const resumable = (r: IncomingHandoff): boolean => !!(r.manifest.harness && r.manifest.session_id && r.manifest.transcript);
