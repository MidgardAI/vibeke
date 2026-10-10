// New agent sheet logic (no DOM): worktree branch names, folder shortcuts, "Again" and the
// `agent.start` parameters. The sheet in components/new-sheet.tsx only renders this.

import type { HarnessInfo, Workspace } from '@vibeke/core';

// ---- branch names ----------------------------------------------------------------------------

const ADJECTIVES = ['brisk', 'calm', 'clever', 'eager', 'gentle', 'keen', 'lively', 'quiet', 'rapid', 'sunny', 'swift', 'tidy', 'bold', 'bright', 'cosy', 'fresh'];
const NOUNS = ['otter', 'falcon', 'maple', 'harbor', 'comet', 'lantern', 'meadow', 'pebble', 'river', 'badger', 'cedar', 'heron', 'orchid', 'summit', 'willow', 'fjord'];

/** `agent/<adjective>-<noun>-<4 hex>`; `rng` returns [0, 1) (Math.random by default). */
export function generateBranchName(rng: () => number = Math.random): string {
  const pick = <T>(list: readonly T[]): T => list[Math.min(list.length - 1, Math.floor(rng() * list.length))]!;
  const hex = Math.min(0xffff, Math.floor(rng() * 0x10000)).toString(16).padStart(4, '0');
  return `agent/${pick(ADJECTIVES)}-${pick(NOUNS)}-${hex}`;
}

export type BranchProblem = 'empty' | 'long' | 'chars' | 'dots' | 'slashes' | 'edge' | 'lock' | 'at';

/** A subset of `git check-ref-format` for branch names; null = acceptable. */
export function branchProblem(name: string): BranchProblem | null {
  if (name === '') return 'empty';
  if (name.length > 200) return 'long';
  // eslint-disable-next-line no-control-regex
  if (/[\s\x00-\x1f\x7f~^:?*[\\]/.test(name)) return 'chars';
  if (name === '@' || name.includes('@{')) return 'at';
  if (name.includes('..')) return 'dots';
  if (name.includes('//')) return 'slashes';
  if (name.startsWith('-') || name.startsWith('/') || name.endsWith('/') || name.endsWith('.')) return 'edge';
  for (const part of name.split('/')) {
    if (part.startsWith('.')) return 'dots';
    if (part.endsWith('.lock')) return 'lock';
  }
  return null;
}

// ---- folders ---------------------------------------------------------------------------------

export const MAX_FOLDERS = 8;

export interface HostFolders {
  favorites: string[];
  recent: string[];
}

export const EMPTY_FOLDERS: HostFolders = { favorites: [], recent: [] };

/** A typed folder path as sent to the host: trimmed, no trailing slash (except the root). */
export function normalizeFolder(path: string): string {
  const p = path.trim().replace(/\/+$/, '');
  return p === '' && path.trim().startsWith('/') ? '/' : p;
}

/** The last path segment, for a chip label. */
export function folderLabel(path: string): string {
  const p = normalizeFolder(path);
  if (p === '/' || p === '~') return p;
  return p.slice(p.lastIndexOf('/') + 1) || p;
}

/** `path` first, duplicates removed, at most MAX_FOLDERS. */
export function pushRecent(list: readonly string[], path: string): string[] {
  const p = normalizeFolder(path);
  if (!p) return [...list];
  return [p, ...list.filter((x) => x !== p)].slice(0, MAX_FOLDERS);
}

/** Adds `path` to the favourites, or removes it when it is there. A full list refuses new entries. */
export function toggleFavorite(list: readonly string[], path: string): string[] {
  const p = normalizeFolder(path);
  if (!p) return [...list];
  if (list.includes(p)) return list.filter((x) => x !== p);
  return list.length >= MAX_FOLDERS ? [...list] : [...list, p];
}

/** Recent folders that are not favourites, for the second chip row. */
export function recentOnly(f: HostFolders): string[] {
  return f.recent.filter((p) => !f.favorites.includes(p));
}

// ---- last start ("Again") --------------------------------------------------------------------

export type Where = { kind: 'workspace'; id: string } | { kind: 'folder'; cwd: string };

export interface LastStart {
  harness: string;
  where: Where;
  worktree: boolean;
}

export const sameWhere = (a: Where | null, b: Where | null): boolean =>
  !!a && !!b && a.kind === b.kind && (a.kind === 'workspace' ? a.id === (b as typeof a).id : a.cwd === (b as typeof a).cwd);

/** The last start if it can be repeated now (harness installed, workspace still there), else null. */
export function resolveAgain(last: LastStart | undefined, workspaces: readonly Pick<Workspace, 'id'>[], harnesses: readonly HarnessInfo[]): LastStart | null {
  if (!last) return null;
  if (!harnesses.some((h) => h.id === last.harness && h.version_detected)) return null;
  if (last.where.kind === 'workspace' && !workspaces.some((w) => w.id === (last.where as { id: string }).id)) return null;
  return last;
}

// ---- start parameters ------------------------------------------------------------------------

export interface StartInput {
  harness: string;
  prompt: string;
  where: Where;
  worktree: { branch: string; base?: string } | null;
}

export interface StartParams {
  harness: string;
  prompt?: string;
  workspace?: string;
  new_workspace?: { cwd: string };
  worktree?: { branch: string; base?: string };
}

export function buildStartParams(i: StartInput): StartParams {
  const prompt = i.prompt.trim();
  const p: StartParams = { harness: i.harness, ...(prompt ? { prompt } : {}) };
  if (i.where.kind === 'folder') p.new_workspace = { cwd: normalizeFolder(i.where.cwd) };
  else p.workspace = i.where.id;
  if (i.worktree) p.worktree = { branch: i.worktree.branch.trim(), ...(i.worktree.base ? { base: i.worktree.base } : {}) };
  return p;
}

// ---- retries ---------------------------------------------------------------------------------

export interface OpIdSlot {
  key: string;
  id: string;
}

/**
 * The `op_id` for a submission: a retry of the same parameters keeps the id of the first try (the
 * gateway then returns that result instead of starting a second agent); changed parameters get a
 * new id.
 */
export function opIdFor(slot: OpIdSlot | null, params: unknown, make: () => string): OpIdSlot {
  const key = JSON.stringify(params);
  return slot && slot.key === key ? slot : { key, id: make() };
}

export function newOpId(): string {
  const c = (globalThis as { crypto?: { randomUUID?: () => string } }).crypto;
  if (c?.randomUUID) return c.randomUUID();
  return `op-${Date.now().toString(36)}-${Math.random().toString(36).slice(2, 10)}`;
}
