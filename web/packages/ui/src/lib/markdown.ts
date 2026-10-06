// A small, sanitizing Markdown parser for plans and transcripts (spec 16 §9.3): raw HTML is never
// interpreted (it stays literal text), and links are kept only for http(s) URLs. The output is an
// AST that components/markdown.tsx renders with React elements, never innerHTML.

export type Inline =
  | { t: 'text'; v: string }
  | { t: 'code'; v: string }
  | { t: 'strong'; c: Inline[] }
  | { t: 'em'; c: Inline[] }
  | { t: 'del'; c: Inline[] }
  | { t: 'link'; href: string; c: Inline[] };

export type Block =
  | { t: 'h'; level: number; c: Inline[] }
  | { t: 'p'; c: Inline[] }
  | { t: 'code'; lang: string; v: string }
  | { t: 'quote'; c: Block[] }
  | { t: 'list'; ordered: boolean; start: number; items: { checked: boolean | null; c: Block[] }[] }
  | { t: 'hr' }
  /** Literal text (nesting/work limits reached): shown as-is, line breaks kept. */
  | { t: 'raw'; v: string };

/** Deepest quote/list/emphasis nesting that is still parsed; deeper content stays literal. */
export const MAX_DEPTH = 16;
/** Work budget per document (characters scanned, roughly); beyond it the input is shown literally. */
export const MAX_WORK = 4_000_000;

export class Budget {
  work = 0;
  charge(n: number): void {
    this.work += n;
    if (this.work > MAX_WORK) throw new BudgetExceeded();
  }
}
class BudgetExceeded extends Error {}

/** Only absolute http(s) links survive. */
export function safeHref(href: string): string | null {
  try {
    const u = new URL(href.trim());
    return u.protocol === 'http:' || u.protocol === 'https:' ? u.href : null;
  } catch {
    return null;
  }
}

export function parseInline(s: string, depth = 0, budget: Budget = new Budget()): Inline[] {
  if (depth >= MAX_DEPTH) return s ? [{ t: 'text', v: s }] : [];
  budget.charge(s.length + 1);
  const sub = (x: string) => parseInline(x, depth + 1, budget);
  const out: Inline[] = [];
  let text = '';
  const push = (n: Inline) => {
    if (text) out.push({ t: 'text', v: text });
    text = '';
    out.push(n);
  };
  let i = 0;
  while (i < s.length) {
    const c = s[i]!;
    if (c === '\\' && i + 1 < s.length && /[\\`*_{}[\]()#+\-.!~>|]/.test(s[i + 1]!)) {
      text += s[i + 1];
      i += 2;
      continue;
    }
    if (c === '`') {
      budget.charge(s.length - i);
      const m = /^(`+)([\s\S]*?[^`])\1(?!`)/.exec(s.slice(i));
      if (m) {
        push({ t: 'code', v: m[2]!.replace(/^ (.*) $/, '$1') });
        i += m[0].length;
        continue;
      }
    }
    if ((c === '*' || c === '_') && s[i + 1] === c) {
      const end = s.indexOf(c + c, i + 2);
      budget.charge((end < 0 ? s.length : end) - i);
      if (end > i + 2) {
        push({ t: 'strong', c: sub(s.slice(i + 2, end)) });
        i = end + 2;
        continue;
      }
    }
    if (c === '~' && s[i + 1] === '~') {
      const end = s.indexOf('~~', i + 2);
      budget.charge((end < 0 ? s.length : end) - i);
      if (end > i + 2) {
        push({ t: 'del', c: sub(s.slice(i + 2, end)) });
        i = end + 2;
        continue;
      }
    }
    if ((c === '*' || c === '_') && s[i + 1] !== ' ' && s[i + 1] !== undefined) {
      // `_` only at word boundaries (snake_case stays literal)
      const prev = s[i - 1];
      if (c === '*' || prev === undefined || /\W/.test(prev)) {
        let end = s.indexOf(c, i + 1);
        while (end > 0 && c === '_' && s[end + 1] !== undefined && /\w/.test(s[end + 1]!)) end = s.indexOf(c, end + 1);
        budget.charge((end < 0 ? s.length : end) - i);
        if (end > i + 1 && s[end - 1] !== ' ') {
          push({ t: 'em', c: sub(s.slice(i + 1, end)) });
          i = end + 1;
          continue;
        }
      }
    }
    if (c === '[') {
      budget.charge(s.length - i);
      const m = /^\[([^\]]*)\]\(([^)\s]+)(?:\s+"[^"]*")?\)/.exec(s.slice(i));
      if (m) {
        const href = safeHref(m[2]!);
        if (href) push({ t: 'link', href, c: sub(m[1]!) });
        else {
          text += m[1];
        }
        i += m[0].length;
        continue;
      }
    }
    if (c === 'h' && /^https?:\/\//.test(s.slice(i, i + 8))) {
      const m = /^https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"]/.exec(s.slice(i));
      const href = m ? safeHref(m[0]) : null;
      if (m && href) {
        push({ t: 'link', href, c: [{ t: 'text', v: m[0] }] });
        i += m[0].length;
        continue;
      }
    }
    text += c;
    i++;
  }
  if (text) out.push({ t: 'text', v: text });
  return out;
}

const LIST_RE = /^(\s*)([-*+]|\d{1,9}[.)])\s+(.*)$/;

/**
 * Parse untrusted Markdown. Nesting deeper than MAX_DEPTH stays literal, and a document that
 * exceeds the work budget (or trips anything unexpected) is returned as one literal block, so
 * adversarial input can neither overflow the stack nor hang the UI.
 */
export function parseMarkdown(src: string): Block[] {
  const text = src.replace(/\r\n?/g, '\n');
  try {
    return parseBlocks(text.split('\n'), 0, new Budget());
  } catch {
    return text ? [{ t: 'raw', v: text }] : [];
  }
}

function parseBlocks(lines: string[], depth: number, budget: Budget): Block[] {
  if (depth >= MAX_DEPTH) {
    const v = lines.join('\n').trim();
    return v ? [{ t: 'raw', v }] : [];
  }
  const inline = (s: string) => parseInline(s, 0, budget);
  const out: Block[] = [];
  let i = 0;
  let para: string[] = [];
  const endPara = () => {
    if (para.length) out.push({ t: 'p', c: inline(para.join(' ').trim()) });
    para = [];
  };
  while (i < lines.length) {
    const line = lines[i]!;
    budget.charge(line.length + 1);
    const fence = /^\s*(```+|~~~+)\s*([\w+-]*)/.exec(line);
    if (fence) {
      endPara();
      const close = fence[1]!;
      const body: string[] = [];
      i++;
      while (i < lines.length && !lines[i]!.trim().startsWith(close)) body.push(lines[i++]!);
      i++;
      out.push({ t: 'code', lang: fence[2] ?? '', v: body.join('\n') });
      continue;
    }
    if (line.trim() === '') {
      endPara();
      i++;
      continue;
    }
    const h = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
    if (h) {
      endPara();
      out.push({ t: 'h', level: h[1]!.length, c: inline(h[2]!) });
      i++;
      continue;
    }
    if (/^\s*([-*_])(\s*\1){2,}\s*$/.test(line)) {
      endPara();
      out.push({ t: 'hr' });
      i++;
      continue;
    }
    if (/^\s*>/.test(line)) {
      endPara();
      const body: string[] = [];
      while (i < lines.length && /^\s*>/.test(lines[i]!)) body.push(lines[i++]!.replace(/^\s*> ?/, ''));
      out.push({ t: 'quote', c: parseBlocks(body, depth + 1, budget) });
      continue;
    }
    const li = LIST_RE.exec(line);
    if (li) {
      endPara();
      const indent = li[1]!.length;
      const ordered = /\d/.test(li[2]!);
      const start = ordered ? parseInt(li[2]!, 10) : 1;
      const items: { checked: boolean | null; c: Block[] }[] = [];
      while (i < lines.length) {
        const m = LIST_RE.exec(lines[i]!);
        if (!m || m[1]!.length !== indent || /\d/.test(m[2]!) !== ordered) break;
        const body = [m[3]!];
        i++;
        while (i < lines.length) {
          const l = lines[i]!;
          if (l.trim() === '') {
            if (i + 1 < lines.length && /^\s+/.test(lines[i + 1]!) && lines[i + 1]!.search(/\S/) > indent) {
              body.push('');
              i++;
              continue;
            }
            break;
          }
          const sub = LIST_RE.exec(l);
          if (sub && sub[1]!.length <= indent) break;
          if (!sub && l.search(/\S/) <= indent && !/^\s+/.test(l)) {
            body.push(l); // lazy continuation
            i++;
            continue;
          }
          body.push(l.slice(Math.min(l.search(/\S/), indent + 2)));
          i++;
        }
        let checked: boolean | null = null;
        const task = /^\[([ xX])\]\s+/.exec(body[0]!);
        if (task) {
          checked = task[1] !== ' ';
          body[0] = body[0]!.slice(task[0].length);
        }
        items.push({ checked, c: parseBlocks(body, depth + 1, budget) });
      }
      out.push({ t: 'list', ordered, start, items });
      continue;
    }
    para.push(line.trim());
    i++;
  }
  endPara();
  return out;
}
