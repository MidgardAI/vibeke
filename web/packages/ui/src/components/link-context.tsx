// Links in output: what a screen can do with a URL or a file path found in text. A screen that
// can open workspace files provides `LinkOps`; without it, paths stay plain text and URLs still
// open in the device browser.

import { createContext, useContext, type ReactNode } from 'react';
import { findLinks, parsePathRef } from '../lib/linkify';

export interface LinkOps {
  /** A path as written in text → the workspace file it names (checked to exist), or null. */
  fileFor(raw: string): string | null;
  /** Show a workspace file in the viewer, at `line` when given. */
  openFile(path: string, line?: number): void;
  /** Open an http(s) URL in the device browser. */
  openUrl(url: string): void;
  /** A relative Markdown link (`docs/a.md`) → the file it names; default: `fileFor`. */
  hrefFile?(href: string): string | null;
}

export const LinkContext = createContext<LinkOps | null>(null);
export const useLinks = (): LinkOps | null => useContext(LinkContext);

/** An http(s) URL link; the caller has already checked the scheme. */
export function UrlLink({ url, ops, children }: { url: string; ops: Pick<LinkOps, 'openUrl'>; children: ReactNode }) {
  return (
    <a
      href={url}
      target="_blank"
      rel="noopener noreferrer"
      data-link="url"
      className="vk-link"
      onClick={(e) => {
        e.preventDefault();
        ops.openUrl(url);
      }}
    >
      {children}
    </a>
  );
}

/** A link to a workspace file (`path`, at `line`). */
export function FileLink({ path, line, ops, children }: { path: string; line?: number; ops: Pick<LinkOps, 'openFile'>; children: ReactNode }) {
  const open = () => ops.openFile(path, line);
  return (
    <span
      role="link"
      tabIndex={0}
      data-link="file"
      title={line ? `${path}:${line}` : path}
      className="vk-link"
      onClick={(e) => {
        e.preventDefault();
        open();
      }}
      onKeyDown={(e) => {
        if (e.key === 'Enter') {
          e.preventDefault();
          open();
        }
      }}
    >
      {children}
    </span>
  );
}

/** Plain text with its URLs and existing file paths turned into links. */
export function linkifyText(text: string, ops: LinkOps): ReactNode[] {
  const spans = findLinks(text);
  if (!spans.length) return [text];
  const out: ReactNode[] = [];
  let pos = 0;
  for (const s of spans) {
    const shown = text.slice(s.start, s.end);
    let node: ReactNode;
    if (s.kind === 'url') {
      node = (
        <UrlLink key={s.start} url={s.url} ops={ops}>
          {shown}
        </UrlLink>
      );
    } else {
      const file = ops.fileFor(s.raw);
      if (!file) continue;
      node = (
        <FileLink key={s.start} path={file} line={s.line} ops={ops}>
          {shown}
        </FileLink>
      );
    }
    if (s.start > pos) out.push(text.slice(pos, s.start));
    out.push(node);
    pos = s.end;
  }
  if (pos < text.length) out.push(text.slice(pos));
  return out;
}

/** The file a relative Markdown link names (`src/a.ts`, `src/a.ts:12`, `../b.md#L7`), or null. */
export function resolveHref(ops: LinkOps, href: string): { path: string; line?: number } | null {
  const hashLine = /#L(\d+)/.exec(href);
  const bare = href.replace(/[?#].*$/, '');
  if (ops.hrefFile) {
    const path = ops.hrefFile(bare);
    return path ? { path, line: hashLine ? Number(hashLine[1]) : undefined } : null;
  }
  const ref = parsePathRef(bare);
  const path = ref ? ops.fileFor(ref.raw) : null;
  if (!path) return null;
  return { path, line: ref!.line ?? (hashLine ? Number(hashLine[1]) : undefined) };
}
