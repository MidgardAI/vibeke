// Unified-diff parsing and a deliberately tiny syntax highlighter for the Changes view. Tokens are
// classes, not HTML; the diff component renders them as spans.

export type DiffLineKind = 'add' | 'del' | 'ctx' | 'hunk' | 'meta';

export interface DiffLine {
  kind: DiffLineKind;
  text: string;
  oldNo: number | null;
  newNo: number | null;
}

export function parseDiff(diff: string): DiffLine[] {
  const out: DiffLine[] = [];
  let o = 0;
  let n = 0;
  let inHunk = false;
  for (const raw of diff.split('\n')) {
    const h = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@(.*)$/.exec(raw);
    if (h) {
      o = Number(h[1]);
      n = Number(h[2]);
      inHunk = true;
      out.push({ kind: 'hunk', text: raw, oldNo: null, newNo: null });
      continue;
    }
    if (!inHunk || raw.startsWith('diff --git') || raw.startsWith('index ')) {
      if (raw.startsWith('diff --git')) inHunk = false;
      if (raw !== '') out.push({ kind: 'meta', text: raw, oldNo: null, newNo: null });
      continue;
    }
    if (raw.startsWith('+')) out.push({ kind: 'add', text: raw.slice(1), oldNo: null, newNo: n++ });
    else if (raw.startsWith('-')) out.push({ kind: 'del', text: raw.slice(1), oldNo: o++, newNo: null });
    else if (raw.startsWith('\\')) out.push({ kind: 'meta', text: raw, oldNo: null, newNo: null });
    else if (raw === '' ) continue;
    else out.push({ kind: 'ctx', text: raw.slice(1), oldNo: o++, newNo: n++ });
  }
  return out;
}

export type TokenKind = 'kw' | 'str' | 'num' | 'com' | 'pun' | 'fn' | 'plain';
export interface Token {
  k: TokenKind;
  v: string;
}

const KEYWORDS = new Set(
  (
    'abstract as async await break case catch class const continue def default defer del delete do elif else enum ' +
    'export extends false final finally fn for from func function go if impl import in interface is let loop match ' +
    'mod module mut new nil none null package pass pub raise return self static struct super switch this throw ' +
    'trait true try type typeof use var void where while with yield None True False'
  ).split(' '),
);

const HASH_COMMENT = new Set(['py', 'rb', 'sh', 'bash', 'zsh', 'toml', 'yaml', 'yml', 'r', 'pl', 'nix', 'conf', 'ini', 'mk', 'Makefile', 'dockerfile']);

export const langOf = (path: string): string => {
  const base = path.split('/').pop() ?? path;
  if (base === 'Makefile' || base === 'Dockerfile') return base === 'Makefile' ? 'mk' : 'dockerfile';
  const m = /\.([a-z0-9]+)$/i.exec(base);
  return m ? m[1]!.toLowerCase() : '';
};

/** Tokenize one line. Multi-line strings/comments are not tracked (diffs show fragments anyway). */
export function tokenize(line: string, lang: string): Token[] {
  const out: Token[] = [];
  const hash = HASH_COMMENT.has(lang);
  const slash = !hash && lang !== 'md' && lang !== 'txt' && lang !== '';
  let i = 0;
  const push = (k: TokenKind, v: string) => {
    const last = out[out.length - 1];
    if (last && last.k === k && k === 'plain') last.v += v;
    else out.push({ k, v });
  };
  while (i < line.length) {
    const rest = line.slice(i);
    let m: RegExpExecArray | null;
    if ((hash && rest[0] === '#') || (slash && rest.startsWith('//')) || ((lang === 'sql' || lang === 'lua') && rest.startsWith('--'))) {
      push('com', rest);
      break;
    }
    if (slash && rest.startsWith('/*')) {
      const end = rest.indexOf('*/', 2);
      const v = end < 0 ? rest : rest.slice(0, end + 2);
      push('com', v);
      i += v.length;
      continue;
    }
    if ((m = /^(["'`])(?:\\.|(?!\1).)*\1?/.exec(rest))) {
      push('str', m[0]);
      i += m[0].length;
      continue;
    }
    if ((m = /^(?:0x[\da-f_]+|\d[\d_]*(?:\.\d+)?(?:e[+-]?\d+)?)\b/i.exec(rest))) {
      push('num', m[0]);
      i += m[0].length;
      continue;
    }
    if ((m = /^[A-Za-z_$][\w$]*/.exec(rest))) {
      const w = m[0];
      if (KEYWORDS.has(w)) push('kw', w);
      else if (line[i + w.length] === '(') push('fn', w);
      else push('plain', w);
      i += w.length;
      continue;
    }
    if ((m = /^[{}()[\];,.:<>=+\-*/%!&|^~?]+/.exec(rest))) {
      push('pun', m[0]);
      i += m[0].length;
      continue;
    }
    push('plain', rest[0]!);
    i++;
  }
  return out;
}
