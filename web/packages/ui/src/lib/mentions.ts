// Mention names (spec 19): the pure logic behind `@` completion in the composer. The name rules
// and `suggestMention` mirror crates/vk-proto/src/mention.rs, so the web app proposes the same
// names as the terminal interface.

import type { Dashboard, MentionEntry } from '@vibeke/core';
import { runForPane } from './tree';

/** Longest mention name. */
export const MAX_LEN = 32;

/** Words a name may not take (target keywords and broadcast-looking words). */
export const RESERVED: readonly string[] = ['current', 'focused', 'all', 'everyone', 'here', 'me', 'self', 'human', 'user', 'vibeke'];

/** Filler words left out of a name built from a label. */
const FILLER: readonly string[] = ['a', 'an', 'the', 'and', 'or', 'of', 'to', 'in', 'on', 'for', 'with', 'at', 'by', 'is'];

/**
 * Whether `name` (without `@`) is a valid mention name: `[a-z][a-z0-9_-]{0,31}` that does not end
 * with `-` or `_`, and not reserved. A trailing `-` or `_` after `@name` is punctuation.
 */
export const validMention = (name: string): boolean => /^[a-z](?:[a-z0-9_-]{0,30}[a-z0-9])?$/.test(name) && !RESERVED.includes(name);

/** `label` as a valid name, or null when nothing usable is left. */
function slug(label: string): string | null {
  const words = label
    .split(/[^A-Za-z0-9]+/)
    .filter((w) => w)
    .map((w) => w.toLowerCase());
  const meaningful = words.filter((w) => !FILLER.includes(w));
  const use = meaningful.length ? meaningful : words;
  let out = '';
  for (const w of use.slice(0, 3)) {
    const next = out === '' ? w : `${out}-${w}`;
    if (next.length > 20 && out !== '') break;
    out = next;
  }
  out = out.replace(/^[^a-z]+/, '').slice(0, MAX_LEN).replace(/[-_]+$/, '');
  return out && validMention(out) ? out : null;
}

/**
 * A name for a pane that has none yet: the first words of its label, lowercase ASCII, filler
 * words dropped, cut near 20 characters. `fallback` is used when the label gives nothing usable
 * (the harness, or the base name of the cwd for a shell). A `-2`, `-3`… suffix makes it unique
 * among the names `taken` reports.
 */
export function suggestMention(label: string, fallback: string, taken: (name: string) => boolean): string {
  const base = slug(label) ?? slug(fallback) ?? 'agent';
  if (!taken(base)) return base;
  for (let n = 2; ; n++) {
    const suffix = `-${n}`;
    const stem = base.slice(0, MAX_LEN - suffix.length).replace(/[-_]+$/, '');
    const candidate = `${stem}${suffix}`;
    if (!taken(candidate)) return candidate;
  }
}

/** The `@word` being typed at the end of the text: `fix @re` → `{query: 're', start: 4}`. */
export function atQuery(text: string): { query: string; start: number } | null {
  const m = /(^|[\s([{"'])@([a-z0-9_-]*)$/.exec(text);
  if (!m) return null;
  return { query: m[2]!, start: m.index + m[1]!.length };
}

/** A pane that could be mentioned, as the composer's host passes it in. */
export interface MentionablePane {
  id: string;
  /** The sidebar label of the pane. */
  label: string;
  /** Harness for an agent pane, else the base name of the cwd. */
  fallback: string;
  /** `idle`, `busy`, `needs_input`, `exited` or `shell`. */
  state: string;
}

export interface MentionRow {
  pane: string;
  /** The name that gets inserted (a proposal when `named` is false). */
  name: string;
  named: boolean;
  label: string;
  state: string;
  harness: string | null;
}

/**
 * Rows for the completion list: the named panes of the workspace (without the pane being typed
 * in), then the unnamed ones with a proposed name that does not clash with any existing name or
 * with another proposal.
 */
export function mentionRows(named: readonly MentionEntry[], panes: readonly MentionablePane[], self: string): MentionRow[] {
  const taken = new Set(named.map((m) => m.name));
  const out: MentionRow[] = named
    .filter((m) => m.pane !== self)
    .map((m) => ({ pane: m.pane, name: m.name, named: true, label: m.label, state: m.state, harness: m.harness ?? null }));
  const namedPanes = new Set(named.map((m) => m.pane));
  for (const p of panes) {
    if (p.id === self || namedPanes.has(p.id)) continue;
    const name = suggestMention(p.label, p.fallback, (n) => taken.has(n));
    taken.add(name);
    out.push({ pane: p.id, name, named: false, label: p.label, state: p.state, harness: null });
  }
  return out;
}

/** Rows matching the typed prefix: names that start with it first, then names, labels or harnesses that contain it. */
export function filterMentions(rows: readonly MentionRow[], query: string): MentionRow[] {
  const q = query.toLowerCase();
  const starts: MentionRow[] = [];
  const has: MentionRow[] = [];
  for (const r of rows) {
    if (r.name.startsWith(q)) starts.push(r);
    else if (q && (r.name.includes(q) || r.label.toLowerCase().includes(q) || (r.harness ?? '').toLowerCase().includes(q))) has.push(r);
  }
  return [...starts, ...has];
}

/** Replace the `@word` being typed (from `start`) with `@name ` . */
export const insertMention = (text: string, start: number, name: string): string => `${text.slice(0, start)}@${name} `;

const baseName = (path: string | null): string => (path ? (path.replace(/\/+$/, '').split('/').pop() ?? '') : '');

/** The panes of a workspace as `@` candidates: label, fallback and state, from the dashboard. */
export function mentionablePanes(d: Dashboard, workspace: string): MentionablePane[] {
  const open = d.interactions.filter((i) => i.status === 'open');
  return d.panes
    .filter((p) => p.workspace === workspace)
    .map((p) => {
      const run = runForPane(d, p.id);
      const label = run ? (p.title ?? run.name ?? run.title ?? run.harness) : (p.title ?? p.auto_title);
      let state = 'shell';
      if (p.exited) state = 'exited';
      else if (run) {
        const exec = run.execution.value;
        state = open.some((i) => i.pane === p.id) ? 'needs_input' : exec === 'idle' ? 'idle' : exec === 'exited' ? 'exited' : 'busy';
      }
      return { id: p.id, label, fallback: run ? run.harness : baseName(p.cwd), state };
    });
}
