// Path-picker logic (no DOM): split what was typed into the folder to list and the name being
// typed, filter that folder's directories, complete and descend. Paths are the host's (`/`-separated,
// `~` for the home folder); the listing comes from `fs.browse`.

import type { BrowseEntry } from '@vibeke/core';
import { fuzzyScore } from './shortcuts';

export interface PathParts {
  /** The folder to list: everything up to and including the last `/` (`~/` when nothing was typed). */
  dir: string;
  /** The partial name after it. */
  prefix: string;
}

/** `~/code/vi` → `{dir: '~/code/', prefix: 'vi'}`; `~` and `''` list the home folder. */
export function splitPath(value: string): PathParts {
  if (value === '' || value === '~') return { dir: '~/', prefix: '' };
  const i = value.lastIndexOf('/');
  // A bare name is looked up in the home folder.
  if (i < 0) return { dir: '~/', prefix: value };
  return { dir: value.slice(0, i + 1), prefix: value.slice(i + 1) };
}

/** The `fs.browse` request for a folder: dot-folders are asked for only when the name starts with `.`. */
export function browseArgs(value: string): { path: string; prefix?: string } {
  const { dir, prefix } = splitPath(value);
  return prefix.startsWith('.') ? { path: dir, prefix: '.' } : { path: dir };
}

/** The folder's directories matching the typed name, best first (all of them, in order, when empty). */
export function filterEntries(entries: readonly BrowseEntry[], prefix: string): BrowseEntry[] {
  if (!prefix) return entries.filter((e) => !e.name.startsWith('.'));
  const scored: { e: BrowseEntry; s: number; i: number }[] = [];
  entries.forEach((e, i) => {
    if (e.name.startsWith('.') && !prefix.startsWith('.')) return;
    const s = fuzzyScore(prefix, e.name);
    if (s >= 0) scored.push({ e, s, i });
  });
  scored.sort((a, b) => b.s - a.s || a.i - b.i);
  return scored.map((x) => x.e);
}

/** Longest common prefix, compared case-insensitively, spelled as in the first string. */
export function commonPrefix(names: readonly string[]): string {
  if (names.length === 0) return '';
  let n = names[0]!.length;
  const first = names[0]!.toLowerCase();
  for (const s of names.slice(1)) {
    const l = s.toLowerCase();
    let i = 0;
    while (i < n && i < l.length && first[i] === l[i]) i++;
    n = i;
  }
  return names[0]!.slice(0, n);
}

/**
 * Tab completion: one match completes to `name/`; several extend the typed name to their common
 * prefix. Null when nothing would change.
 */
export function complete(value: string, entries: readonly BrowseEntry[]): string | null {
  const { dir, prefix } = splitPath(value);
  const p = prefix.toLowerCase();
  const matches = entries.filter((e) => e.name.toLowerCase().startsWith(p) && (!e.name.startsWith('.') || prefix.startsWith('.')));
  if (matches.length === 0) return null;
  if (matches.length === 1) return `${dir}${matches[0]!.name}/`;
  const common = commonPrefix(matches.map((e) => e.name));
  return common.length > prefix.length ? `${dir}${common}` : null;
}

/** Into `name` in the folder being listed. */
export function descend(value: string, name: string): string {
  return `${splitPath(value).dir}${name}/`;
}

/** The listed folder's parent: `~/code/x/` and `~/code/x/pa` → `~/code/`; `/` stays `/`. */
export function parentOf(value: string): string {
  const dir = splitPath(value).dir.replace(/\/+$/, '');
  if (dir === '') return '/';
  if (dir === '~') return '~/';
  const i = dir.lastIndexOf('/');
  return i < 0 ? '~/' : dir.slice(0, i + 1);
}
