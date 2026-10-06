// ANSI → styled segments (spec 16 §9.3 rendering safety): terminal text is never injected as
// HTML. SGR colour/attribute sequences become segment styles; every other escape (cursor moves,
// OSC titles/hyperlinks, DCS, charset switches) is dropped so nothing raw reaches the screen.

export interface Style {
  fg?: string;
  bg?: string;
  bold?: boolean;
  dim?: boolean;
  italic?: boolean;
  underline?: boolean;
  inverse?: boolean;
  strike?: boolean;
}

export interface Segment {
  text: string;
  style: Style;
}

/** xterm 16-colour palette as CSS variables so themes can retint it. */
const BASIC = ['black', 'red', 'green', 'yellow', 'blue', 'magenta', 'cyan', 'white'];
const basic = (i: number, bright: boolean) => `var(--ansi-${bright ? 'bright-' : ''}${BASIC[i]})`;

export function x256(n: number): string {
  if (n < 8) return basic(n, false);
  if (n < 16) return basic(n - 8, true);
  if (n < 232) {
    const v = n - 16;
    const c = [Math.floor(v / 36), Math.floor(v / 6) % 6, v % 6].map((x) => (x === 0 ? 0 : 55 + x * 40));
    return `rgb(${c[0]},${c[1]},${c[2]})`;
  }
  const g = 8 + (n - 232) * 10;
  return `rgb(${g},${g},${g})`;
}

function applySgr(params: number[], s: Style): Style {
  const st: Style = { ...s };
  if (params.length === 0) params = [0];
  for (let i = 0; i < params.length; i++) {
    const p = params[i]!;
    if (p === 0) {
      for (const k of Object.keys(st)) delete st[k as keyof Style];
    } else if (p === 1) st.bold = true;
    else if (p === 2) st.dim = true;
    else if (p === 3) st.italic = true;
    else if (p === 4) st.underline = true;
    else if (p === 7) st.inverse = true;
    else if (p === 9) st.strike = true;
    else if (p === 22) st.bold = st.dim = false;
    else if (p === 23) st.italic = false;
    else if (p === 24) st.underline = false;
    else if (p === 27) st.inverse = false;
    else if (p === 29) st.strike = false;
    else if (p >= 30 && p <= 37) st.fg = basic(p - 30, false);
    else if (p === 39) delete st.fg;
    else if (p >= 40 && p <= 47) st.bg = basic(p - 40, false);
    else if (p === 49) delete st.bg;
    else if (p >= 90 && p <= 97) st.fg = basic(p - 90, true);
    else if (p >= 100 && p <= 107) st.bg = basic(p - 100, true);
    else if (p === 38 || p === 48) {
      const key = p === 38 ? 'fg' : 'bg';
      if (params[i + 1] === 5 && params[i + 2] !== undefined) {
        st[key] = x256(params[i + 2]!);
        i += 2;
      } else if (params[i + 1] === 2 && params[i + 4] !== undefined) {
        st[key] = `rgb(${params[i + 2]},${params[i + 3]},${params[i + 4]})`;
        i += 4;
      }
    }
  }
  return st;
}

const sameStyle = (a: Style, b: Style) => JSON.stringify(a) === JSON.stringify(b);

/** Parse text with ANSI escapes into lines of styled segments. */
export function parseAnsi(input: string): Segment[][] {
  const lines: Segment[][] = [[]];
  let style: Style = {};
  let buf = '';
  const flush = () => {
    if (!buf) return;
    const line = lines[lines.length - 1]!;
    const last = line[line.length - 1];
    if (last && sameStyle(last.style, style)) last.text += buf;
    else line.push({ text: buf, style });
    buf = '';
  };
  let i = 0;
  while (i < input.length) {
    const c = input[i]!;
    if (c === '\x1b') {
      flush();
      const n = input[i + 1];
      if (n === '[') {
        // CSI: params then a final byte in @..~
        let j = i + 2;
        while (j < input.length && !/[@-~]/.test(input[j]!)) j++;
        const body = input.slice(i + 2, j);
        if (input[j] === 'm' && /^[\d;:]*$/.test(body)) {
          style = applySgr(
            body === '' ? [] : body.split(/[;:]/).map((x) => (x === '' ? 0 : Number(x))),
            style,
          );
        }
        i = j + 1;
      } else if (n === ']' || n === 'P' || n === '_' || n === '^') {
        // OSC / DCS / APC / PM: until BEL or ST (ESC \)
        let j = i + 2;
        while (j < input.length && input[j] !== '\x07' && !(input[j] === '\x1b' && input[j + 1] === '\\')) j++;
        i = input[j] === '\x07' ? j + 1 : j + 2;
      } else if (n === '(' || n === ')' || n === '#' || n === '%') {
        i += 3;
      } else {
        i += 2;
      }
      continue;
    }
    if (c === '\n') {
      flush();
      lines.push([]);
    } else if (c === '\r') {
      // ignore bare CR (screens are already laid out)
    } else if (c === '\t') {
      buf += '    ';
    } else if (c < ' ' || c === '\x7f') {
      // other C0 controls are not printable
    } else {
      buf += c;
    }
    i++;
  }
  flush();
  return lines;
}

/** Plain text with all escapes removed. */
export const stripAnsi = (s: string): string =>
  parseAnsi(s)
    .map((l) => l.map((x) => x.text).join(''))
    .join('\n');
