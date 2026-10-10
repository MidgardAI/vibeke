// Links in output text: http(s) URLs and file paths (`src/a.ts:42`). Pure scanning; whether a
// path exists in the workspace is decided elsewhere (lib/path-index.ts), and only existing paths
// become links. URLs are limited to http(s) (the same rule as the Markdown renderer).

import { safeHref } from './markdown';

export interface UrlSpan {
  kind: 'url';
  start: number;
  end: number;
  url: string;
}

export interface PathSpan {
  kind: 'path';
  start: number;
  end: number;
  /** The path as written, without a `:line` suffix. */
  raw: string;
  line?: number;
  col?: number;
}

export type LinkSpan = UrlSpan | PathSpan;

const URL_RE = /https?:\/\/[^\s<>"'`\\^{}|]+/g;
/** Characters a path token may be made of (a token ends at anything else). */
const TOKEN_RE = /[^\s"'`<>()[\]{}|,;*]+/g;
const PATH_CHARS = /^[\w@.+~/-]+$/;
const EXT_RE = /[^/.]\.[A-Za-z][A-Za-z0-9]{0,7}$/;
/** Most links scanned in one text; more is output noise, not navigation. */
const MAX_SPANS = 400;

/** Trailing punctuation is not part of a URL; a closing bracket stays only when it is balanced. */
function trimUrl(u: string): string {
  let s = u;
  for (;;) {
    const last = s[s.length - 1];
    if (last && '.,;:!?\'"'.includes(last)) s = s.slice(0, -1);
    else if (last === ')' && count(s, ')') > count(s, '(')) s = s.slice(0, -1);
    else if (last === ']' && count(s, ']') > count(s, '[')) s = s.slice(0, -1);
    else return s;
  }
}

function count(s: string, ch: string): number {
  let n = 0;
  for (const c of s) if (c === ch) n++;
  return n;
}

/** Is `s` shaped like a file path (a directory part or a file extension)? */
export function looksLikePath(s: string): boolean {
  if (!s || s.length > 300 || !PATH_CHARS.test(s)) return false;
  if (s.startsWith('-') || s.endsWith('/') || s.includes('//') || s === '.' || s === '..') return false;
  const slash = s.includes('/');
  const ext = EXT_RE.test(s);
  if (!slash && !ext) return false;
  const last = s.slice(s.lastIndexOf('/') + 1);
  return last.length > 0 && last !== '.' && last !== '..' && /[A-Za-z0-9]/.test(last);
}

/** Split `src/a.ts:42:7` into the path and position. Returns null when it is not path-shaped. */
export function parsePathRef(token: string): { raw: string; line?: number; col?: number } | null {
  const m = /^(.*?)(?::(\d{1,7})(?::(\d{1,5}))?)?$/.exec(token);
  if (!m) return null;
  const raw = m[1]!;
  if (!looksLikePath(raw)) return null;
  const out: { raw: string; line?: number; col?: number } = { raw };
  if (m[2]) out.line = Number(m[2]);
  if (m[3]) out.col = Number(m[3]);
  return out;
}

/** URL and path spans of `text`, in order, never overlapping. */
export function findLinks(text: string): LinkSpan[] {
  const spans: LinkSpan[] = [];
  if (!text || text.length > 200_000) return spans;
  const taken: [number, number][] = [];
  for (const m of text.matchAll(URL_RE)) {
    const trimmed = trimUrl(m[0]);
    const url = safeHref(trimmed);
    if (!url) continue;
    spans.push({ kind: 'url', start: m.index!, end: m.index! + trimmed.length, url });
    taken.push([m.index!, m.index! + m[0].length]);
    if (spans.length >= MAX_SPANS) return spans;
  }
  for (const m of text.matchAll(TOKEN_RE)) {
    const start = m.index!;
    if (taken.some(([a, b]) => start < b && start + m[0].length > a)) continue;
    // Strip trailing sentence punctuation (`see src/a.ts.`), but keep a `:line` suffix.
    let tok = m[0];
    while (tok.length && '.:!?'.includes(tok[tok.length - 1]!)) tok = tok.slice(0, -1);
    if (!tok) continue;
    // `@scope/pkg` is a package name, not a file.
    if (tok.startsWith('@')) continue;
    const ref = parsePathRef(tok);
    if (!ref) continue;
    let end = start + tok.length;
    // `src/a.ts(42,7)` (TypeScript style).
    if (ref.line === undefined) {
      const p = /^\((\d{1,7})(?:,(\d{1,5}))?\)/.exec(text.slice(end, end + 16));
      if (p) {
        ref.line = Number(p[1]);
        if (p[2]) ref.col = Number(p[2]);
        end += p[0].length;
      }
    }
    spans.push({ kind: 'path', start, end, ...ref });
    if (spans.length >= MAX_SPANS) break;
  }
  return spans.sort((a, b) => a.start - b.start);
}

/** Distinct path texts in `text` (to check against the workspace), at most `limit`. */
export function extractPathRefs(text: string, limit = 300): string[] {
  const seen = new Set<string>();
  for (const s of findLinks(text)) {
    if (s.kind === 'path') {
      seen.add(s.raw);
      if (seen.size >= limit) break;
    }
  }
  return [...seen];
}

// ---- workspace paths ----------------------------------------------------------------------------

/** Resolve `.` and `..`; null when the path leaves the root (or is empty). Never starts with `/`. */
export function normalizePath(p: string): string | null {
  const out: string[] = [];
  for (const seg of p.split('/')) {
    if (seg === '' || seg === '.') continue;
    if (seg === '..') {
      if (!out.length) return null;
      out.pop();
    } else out.push(seg);
  }
  return out.length ? out.join('/') : null;
}

/** `abs` relative to `root` (both absolute), '' for the root itself; null when outside. */
export function relativeTo(root: string, abs: string): string | null {
  const r = root.replace(/\/+$/, '');
  if (abs === r) return '';
  return abs.startsWith(`${r}/`) ? abs.slice(r.length + 1) : null;
}

/**
 * Workspace-relative paths a written path may mean, best first. Terminal output is usually
 * relative to the pane's directory; `repoRoot` is the root `fs.list` / `fs.read` use.
 */
export function pathCandidates(raw: string, o: { repoRoot?: string | null; cwd?: string | null }): string[] {
  const out: string[] = [];
  const add = (p: string | null) => {
    if (p && !out.includes(p)) out.push(p);
  };
  if (raw.startsWith('~')) return out;
  if (raw.startsWith('/')) {
    if (o.repoRoot) {
      const rel = relativeTo(o.repoRoot, raw);
      if (rel) add(normalizePath(rel));
    }
    return out;
  }
  const base = o.repoRoot && o.cwd ? relativeTo(o.repoRoot, o.cwd) : null;
  if (base) add(normalizePath(`${base}/${raw}`));
  add(normalizePath(raw));
  return out;
}
