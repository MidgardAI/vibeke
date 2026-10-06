// One-line summaries of agent tool calls for the conversation: a short tool label and the part
// of the input that matters (a shell command, a path, a pattern, a URL…). The transcript gives
// the input as one line of JSON cut at 160 characters, so fields are read with a tolerant scan
// that copes with the cut instead of JSON.parse alone.

export type ToolKind = 'shell' | 'read' | 'edit' | 'write' | 'search' | 'web' | 'task' | 'todo' | 'plan' | 'other';

export interface ToolSummary {
  kind: ToolKind;
  /** Short display name: `Shell`, `Read`, `Edit`… */
  label: string;
  /** The interesting argument, one line (may be empty). */
  detail: string;
}

/** A string field of a (possibly truncated) JSON object. */
export function jsonField(src: string, key: string): string | null {
  const re = new RegExp(`"${key.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}"\\s*:\\s*"((?:\\\\.|[^"\\\\])*\\\\?)("|$)`);
  const m = re.exec(src);
  if (!m) return null;
  return unescape(m[1]!);
}

/** A string-array field of a (possibly truncated) JSON object. */
export function jsonArrayField(src: string, key: string): string[] | null {
  const re = new RegExp(`"${key}"\\s*:\\s*\\[`);
  const m = re.exec(src);
  if (!m) return null;
  const out: string[] = [];
  const s = src.slice(m.index + m[0].length);
  const str = /\s*"((?:\\.|[^"\\])*\\?)("|$)\s*(,|\]|$)/y;
  let i = 0;
  for (;;) {
    str.lastIndex = i;
    const x = str.exec(s);
    if (!x) break;
    out.push(unescape(x[1]!));
    if (x[3] !== ',') break;
    i = str.lastIndex;
  }
  return out;
}

function unescape(raw: string): string {
  // Drop a dangling backslash left by the cut, then decode as a JSON string.
  const body = raw.replace(/(^|[^\\])(\\\\)*\\$/, (s) => s.slice(0, -1));
  try {
    return JSON.parse(`"${body}"`) as string;
  } catch {
    return body.replace(/\\(.)/g, '$1');
  }
}

const oneLine = (s: string): string => s.replace(/\s+/g, ' ').trim();

/** `bash -lc "…"` → the script; other argv joined. */
export function shellCommand(argv: string[]): string {
  if (argv.length >= 3 && /(^|\/)(ba|z|da)?sh$/.test(argv[0]!) && /^-\w*c\w*$/.test(argv[1]!)) return argv.slice(2).join(' ');
  return argv.join(' ');
}

/** File paths named in an apply_patch body. */
export function patchPaths(patch: string): string[] {
  const out: string[] = [];
  const re = /\*\*\* (?:Add|Update|Delete) File: ([^\n*]+?)(?=\s*(?:\n|\*\*\*|$))/g;
  let m: RegExpExecArray | null;
  while ((m = re.exec(patch))) if (!out.includes(m[1]!.trim())) out.push(m[1]!.trim());
  return out;
}

/** `/repo/src/a.ts` with cwd `/repo` → `src/a.ts`. */
export function relPath(p: string, cwd?: string | null): string {
  if (cwd) {
    const base = cwd.endsWith('/') ? cwd : `${cwd}/`;
    if (p.startsWith(base)) return p.slice(base.length);
  }
  return p;
}

/** First string value of the object (generic fallback). */
function firstString(src: string): string | null {
  const m = /"[\w-]+"\s*:\s*"((?:\\.|[^"\\])*\\?)("|$)/.exec(src);
  return m ? unescape(m[1]!) : null;
}

const pretty = (tool: string): string => {
  // mcp__server__do_thing → do thing
  const t = tool.startsWith('mcp__') ? (tool.split('__').pop() ?? tool) : tool;
  const s = t.replace(/[_-]+/g, ' ').trim();
  return s ? s[0]!.toUpperCase() + s.slice(1) : tool;
};

export function toolSummary(tool: string | null | undefined, summary: string | null | undefined, cwd?: string | null): ToolSummary {
  const name = (tool ?? '').trim();
  const src = (summary ?? '').trim();
  const f = (k: string) => jsonField(src, k);
  const path = (k: string) => {
    const v = f(k);
    return v ? relPath(v, cwd) : '';
  };
  const shell = (cmd: string): ToolSummary => {
    const c = cmd.trim();
    if (/^apply_patch\b/.test(c)) {
      const ps = patchPaths(c);
      return { kind: 'edit', label: 'Edit', detail: ps.map((p) => relPath(p, cwd)).join(', ') };
    }
    return { kind: 'shell', label: 'Shell', detail: oneLine(c) };
  };

  switch (name) {
    case 'Bash':
    case 'BashOutput':
    case 'Shell':
    case 'shell':
    case 'exec_command':
    case 'local_shell':
    case 'container.exec':
    case '': {
      const argv = jsonArrayField(src, 'command');
      if (argv && argv.length) return shell(shellCommand(argv));
      const cmd = f('command') ?? f('cmd');
      if (cmd !== null) return shell(cmd);
      if (!name) return { kind: 'other', label: 'Tool', detail: src === 'null' ? '' : oneLine(src) };
      return { kind: 'shell', label: 'Shell', detail: oneLine(src) };
    }
    case 'Read':
    case 'read_file':
      return { kind: 'read', label: 'Read', detail: path('file_path') || path('path') };
    case 'Edit':
    case 'MultiEdit':
    case 'NotebookEdit':
    case 'edit_file':
      return { kind: 'edit', label: 'Edit', detail: path('file_path') || path('notebook_path') || path('path') };
    case 'Write':
    case 'write_file':
      return { kind: 'write', label: 'Write', detail: path('file_path') || path('path') };
    case 'apply_patch': {
      const body = f('input') ?? f('patch') ?? src;
      return { kind: 'edit', label: 'Edit', detail: patchPaths(body).map((p) => relPath(p, cwd)).join(', ') };
    }
    case 'Grep':
    case 'grep':
    case 'search': {
      const pat = f('pattern') ?? f('query') ?? '';
      const where = path('path') || f('glob') || '';
      return { kind: 'search', label: 'Grep', detail: [pat, where && `in ${where}`].filter(Boolean).join(' ') };
    }
    case 'Glob':
    case 'LS':
    case 'list_dir':
      return { kind: 'search', label: name === 'Glob' ? 'Glob' : 'List', detail: f('pattern') ?? path('path') };
    case 'WebFetch':
    case 'fetch':
      return { kind: 'web', label: 'Fetch', detail: f('url') ?? '' };
    case 'WebSearch':
    case 'web_search':
      return { kind: 'web', label: 'Search', detail: f('query') ?? '' };
    case 'Task':
    case 'Agent':
    case 'spawn_agent':
    case 'spawn_subagent':
    case 'create_agent':
      return { kind: 'task', label: 'Agent', detail: oneLine(f('description') ?? f('prompt') ?? f('message') ?? '') };
    case 'TodoWrite':
      return { kind: 'todo', label: 'Todos', detail: '' };
    case 'update_plan':
    case 'ExitPlanMode':
      return { kind: 'plan', label: 'Plan', detail: oneLine(f('explanation') ?? f('plan') ?? '') };
    default:
      return { kind: 'other', label: pretty(name), detail: oneLine(firstString(src) ?? (src === 'null' || src === '{}' ? '' : src)) };
  }
}
