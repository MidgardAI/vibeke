// File-type glyphs for trees and diffs: small inline SVGs keyed by extension, no icon font or
// dependency. Colours are muted so a long tree stays calm; folders follow the text colour.

import type { ReactNode } from 'react';
import { cx } from './ui';

export type FileKind =
  | 'ts'
  | 'tsx'
  | 'js'
  | 'rs'
  | 'md'
  | 'json'
  | 'toml'
  | 'yaml'
  | 'css'
  | 'html'
  | 'sh'
  | 'lock'
  | 'image'
  | 'folder'
  | 'default';

const EXT: Record<string, FileKind> = {
  ts: 'ts',
  mts: 'ts',
  cts: 'ts',
  tsx: 'tsx',
  jsx: 'tsx',
  js: 'js',
  mjs: 'js',
  cjs: 'js',
  rs: 'rs',
  md: 'md',
  mdx: 'md',
  markdown: 'md',
  json: 'json',
  jsonc: 'json',
  toml: 'toml',
  ini: 'toml',
  yaml: 'yaml',
  yml: 'yaml',
  css: 'css',
  scss: 'css',
  less: 'css',
  html: 'html',
  htm: 'html',
  svg: 'image',
  png: 'image',
  jpg: 'image',
  jpeg: 'image',
  gif: 'image',
  webp: 'image',
  ico: 'image',
  avif: 'image',
  sh: 'sh',
  bash: 'sh',
  zsh: 'sh',
  fish: 'sh',
  lock: 'lock',
};

const NAMES: Record<string, FileKind> = {
  'cargo.lock': 'lock',
  'bun.lock': 'lock',
  'bun.lockb': 'lock',
  'package-lock.json': 'lock',
  'pnpm-lock.yaml': 'lock',
  'yarn.lock': 'lock',
  makefile: 'sh',
  dockerfile: 'sh',
  justfile: 'sh',
};

/** Which glyph a path gets (by basename, then extension). */
export function fileKind(path: string, dir = false): FileKind {
  if (dir) return 'folder';
  const name = (path.split('/').pop() ?? path).toLowerCase();
  if (NAMES[name]) return NAMES[name];
  const dot = name.lastIndexOf('.');
  if (dot <= 0) return 'default';
  return EXT[name.slice(dot + 1)] ?? 'default';
}

/** A two-letter badge (TS, JS, RS…). */
const badge = (text: string, fill: string, fg = '#fff') => (
  <>
    <rect x="1.5" y="1.5" width="13" height="13" rx="2.5" fill={fill} />
    <text x="8" y="11.1" textAnchor="middle" fontSize="6.6" fontWeight="700" fontFamily="ui-sans-serif, system-ui, sans-serif" fill={fg}>
      {text}
    </text>
  </>
);

const page = (inner: ReactNode, stroke = 'currentColor') => (
  <g fill="none" stroke={stroke} strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
    <path d="M4 1.8h5.2L12.5 5v8.2a1 1 0 0 1-1 1H4a1 1 0 0 1-1-1V2.8a1 1 0 0 1 1-1Z" />
    <path d="M9 1.8V5h3.5" />
    {inner}
  </g>
);

const GLYPHS: Record<FileKind, ReactNode> = {
  ts: badge('TS', '#3178c6'),
  js: badge('JS', '#e6c84a', '#1b1b1b'),
  rs: badge('RS', '#b7653b'),
  tsx: (
    <g fill="none" stroke="#58c4dc" strokeWidth="1.1">
      <ellipse cx="8" cy="8" rx="6.5" ry="2.6" />
      <ellipse cx="8" cy="8" rx="6.5" ry="2.6" transform="rotate(60 8 8)" />
      <ellipse cx="8" cy="8" rx="6.5" ry="2.6" transform="rotate(120 8 8)" />
      <circle cx="8" cy="8" r="1.2" fill="#58c4dc" stroke="none" />
    </g>
  ),
  md: (
    <g fill="none" stroke="#7d8dff" strokeWidth="1.2" strokeLinecap="round" strokeLinejoin="round">
      <rect x="1.5" y="3.5" width="13" height="9" rx="2" />
      <path d="M4 10V6l1.8 2L7.6 6v4M11 6v4M9.6 8.6 11 10l1.4-1.4" />
    </g>
  ),
  json: (
    <g fill="none" stroke="#d6a84b" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
      <path d="M5.5 2.5c-1.5 0-2 .6-2 2v1.6c0 1-.5 1.6-1.5 1.9 1 .3 1.5.9 1.5 1.9v1.6c0 1.4.5 2 2 2M10.5 2.5c1.5 0 2 .6 2 2v1.6c0 1 .5 1.6 1.5 1.9-1 .3-1.5.9-1.5 1.9v1.6c0 1.4-.5 2-2 2" />
    </g>
  ),
  toml: page(<path d="M5 7.5h5.5M5 9.8h5.5M5 12h3" />, '#9a9aa2'),
  yaml: page(<path d="M5 7.5h3M6.5 9.8h4M5 12h5" />, '#cb7a7a'),
  css: badge('#', '#8f5bd6'),
  html: (
    <g fill="none" stroke="#e2714a" strokeWidth="1.4" strokeLinecap="round" strokeLinejoin="round">
      <path d="m5.5 4.5-3.5 3.5 3.5 3.5M10.5 4.5 14 8l-3.5 3.5" />
    </g>
  ),
  sh: (
    <g fill="none" stroke="#6fbf73" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
      <rect x="1.5" y="2.5" width="13" height="11" rx="2" />
      <path d="m4.5 6.5 2 1.5-2 1.5M8 10.5h3.5" />
    </g>
  ),
  lock: (
    <g fill="none" stroke="#9a9aa2" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
      <rect x="3" y="7" width="10" height="7" rx="1.5" />
      <path d="M5.2 7V5.2a2.8 2.8 0 0 1 5.6 0V7" />
    </g>
  ),
  image: (
    <g fill="none" stroke="#5fb3a1" strokeWidth="1.3" strokeLinecap="round" strokeLinejoin="round">
      <rect x="1.8" y="2.8" width="12.4" height="10.4" rx="2" />
      <circle cx="5.6" cy="6.3" r="1.1" />
      <path d="m2.5 12 3.6-3.4 2.4 2.2 2.2-1.8 3 2.6" />
    </g>
  ),
  folder: (
    <g fill="none" stroke="currentColor" strokeWidth="1.25" strokeLinejoin="round">
      <path d="M1.8 4.2a1.2 1.2 0 0 1 1.2-1.2h3l1.5 1.6H13a1.2 1.2 0 0 1 1.2 1.2v6.4a1.2 1.2 0 0 1-1.2 1.2H3a1.2 1.2 0 0 1-1.2-1.2Z" />
    </g>
  ),
  default: page(null, '#9a9aa2'),
};

/** 14px glyph for a file path (or a folder when `dir`). */
export function FileIcon({ path, dir, className }: { path: string; dir?: boolean; className?: string }) {
  const kind = fileKind(path, dir);
  return (
    <svg viewBox="0 0 16 16" aria-hidden className={cx('size-3.5 shrink-0', kind === 'folder' && 'text-muted', className)} data-kind={kind}>
      {GLYPHS[kind]}
    </svg>
  );
}
