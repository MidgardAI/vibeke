// File viewer previews: which files have one, links between Markdown files, and a size-bounded
// tree for JSON. Images (png, jpg, gif, webp) come from `fs.read` with `as: 'image'`.

import { normalizePath } from './linkify';
import { parentDir } from './path-index';

export type PreviewKind = 'markdown' | 'svg' | 'json';

export function previewKind(path: string): PreviewKind | null {
  const ext = /\.([A-Za-z0-9]+)$/.exec(path)?.[1]?.toLowerCase();
  switch (ext) {
    case 'md':
    case 'markdown':
    case 'mdx':
      return 'markdown';
    case 'svg':
      return 'svg';
    case 'json':
      return 'json';
    default:
      return null;
  }
}

const IMAGE_MIMES: Record<string, string> = { png: 'image/png', jpg: 'image/jpeg', jpeg: 'image/jpeg', gif: 'image/gif', webp: 'image/webp' };

/** Is this a file the server can return as an image (by extension)? SVG is text and has its own preview. */
export const isImagePath = (path: string): boolean => {
  const ext = /\.([A-Za-z0-9]+)$/.exec(path)?.[1]?.toLowerCase();
  return !!ext && Object.hasOwn(IMAGE_MIMES, ext);
};

/** A raster image type safe to show from a data URL (never SVG, which can carry scripts). */
export const isRasterMime = (mime: string | null | undefined): mime is string => !!mime && Object.values(IMAGE_MIMES).includes(mime.toLowerCase());

/** `data:` URL for server-provided image bytes; null for another type or text outside the base64 alphabet. */
export function imageDataUrl(mime: string | null | undefined, b64: string | null | undefined): string | null {
  if (!isRasterMime(mime) || !b64 || !/^[A-Za-z0-9+/]+={0,2}$/.test(b64)) return null;
  return `data:${mime.toLowerCase()};base64,${b64}`;
}

/** A cut-off file cannot be previewed as SVG or JSON (it would not parse); Markdown reads fine. */
export const canPreview = (kind: PreviewKind, truncated: boolean): boolean => kind === 'markdown' || !truncated;

/**
 * The workspace path a relative Markdown link points to (`./b.md#top`, `../docs/c.md`,
 * `/README.md` = from the workspace root). Null for URLs, anchors and links that leave the root.
 */
export function resolveRelativeLink(fromPath: string, href: string): string | null {
  let h = href.trim().replace(/[?#].*$/, '');
  if (!h || h.startsWith('//') || /^[a-z][a-z0-9+.-]*:/i.test(h) || h.includes('\\')) return null;
  try {
    h = decodeURIComponent(h);
  } catch {
    return null;
  }
  const base = h.startsWith('/') ? '' : parentDir(fromPath);
  return normalizePath(base ? `${base}/${h}` : h);
}

// ---- JSON ---------------------------------------------------------------------------------------

/** Most values drawn in the tree. */
export const JSON_CAP = 5000;
const JSON_MAX_DEPTH = 64;
const JSON_TEXT_MAX = 300;

export type JsonNode =
  | { kind: 'object' | 'array'; key: string | null; size: number; children: JsonNode[]; omitted: number }
  | { kind: 'string' | 'number' | 'boolean' | 'null'; key: string | null; text: string };

export function parseJson(text: string): { ok: true; value: unknown } | { ok: false } {
  try {
    return { ok: true, value: JSON.parse(text) };
  } catch {
    return { ok: false };
  }
}

/** Values in `v`, counting objects and arrays themselves. Stops counting past `limit`. */
export function countValues(v: unknown, limit = Infinity, depth = 0): number {
  let n = 1;
  if (depth >= JSON_MAX_DEPTH || v === null || typeof v !== 'object') return n;
  for (const c of Array.isArray(v) ? v : Object.values(v)) {
    n += countValues(c, limit - n, depth + 1);
    if (n > limit) break;
  }
  return n;
}

export interface JsonTree {
  root: JsonNode;
  /** Values included in the tree. */
  shown: number;
  /** Values in the document. */
  total: number;
  capped: boolean;
}

/** A tree of at most `cap` values, depth-first; what does not fit is counted in `omitted`. */
export function buildJsonTree(value: unknown, cap = JSON_CAP): JsonTree {
  const total = countValues(value);
  let used = 0;
  const build = (v: unknown, key: string | null, depth: number): JsonNode => {
    used++;
    if (v !== null && typeof v === 'object' && depth < JSON_MAX_DEPTH) {
      const entries: [string | null, unknown][] = Array.isArray(v) ? v.map((x, i) => [String(i), x]) : Object.entries(v);
      const children: JsonNode[] = [];
      for (const [k, c] of entries) {
        if (used >= cap) break;
        children.push(build(c, k, depth + 1));
      }
      return { kind: Array.isArray(v) ? 'array' : 'object', key, size: entries.length, children, omitted: entries.length - children.length };
    }
    if (v !== null && typeof v === 'object') return { kind: 'string', key, text: '…' };
    if (v === null) return { kind: 'null', key, text: 'null' };
    if (typeof v === 'string') return { kind: 'string', key, text: v.length > JSON_TEXT_MAX ? `${v.slice(0, JSON_TEXT_MAX)}…` : v };
    return { kind: typeof v === 'boolean' ? 'boolean' : 'number', key, text: String(v) };
  };
  const root = build(value, null, 0);
  return { root, shown: used, total, capped: total > used };
}
