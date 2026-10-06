// Recording a global shortcut: KeyboardEvent → Electron accelerator. Uses `code` (the physical
// key), because with ⌥ held macOS reports the composed character in `key` (⌥V → "√").

export interface KeyEventLike {
  code: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
}

const NAMED: Record<string, string> = {
  Space: 'Space',
  Tab: 'Tab',
  Enter: 'Enter',
  Backspace: 'Backspace',
  Delete: 'Delete',
  Escape: 'Escape',
  ArrowUp: 'Up',
  ArrowDown: 'Down',
  ArrowLeft: 'Left',
  ArrowRight: 'Right',
  Home: 'Home',
  End: 'End',
  PageUp: 'PageUp',
  PageDown: 'PageDown',
  Minus: '-',
  Equal: '=',
  BracketLeft: '[',
  BracketRight: ']',
  Backslash: '\\',
  Semicolon: ';',
  Quote: "'",
  Comma: ',',
  Period: '.',
  Slash: '/',
  Backquote: '`',
};

/** Null while only modifiers are down, or for combos without a modifier. */
export function acceleratorFromEvent(e: KeyEventLike, mac: boolean): string | null {
  let key: string | null = null;
  const m = /^Key([A-Z])$/.exec(e.code) ?? /^Digit([0-9])$/.exec(e.code) ?? /^(F(?:[1-9]|1[0-9]|2[0-4]))$/.exec(e.code);
  if (m) key = m[1]!;
  else key = NAMED[e.code] ?? null;
  if (!key) return null;
  const mods: string[] = [];
  if (e.ctrlKey) mods.push(mac ? 'Control' : 'CommandOrControl');
  if (e.altKey) mods.push('Alt');
  if (e.shiftKey) mods.push('Shift');
  if (e.metaKey) mods.push(mac ? 'Command' : 'Super');
  // Shift alone would steal ordinary typing system-wide.
  if (mods.length === 0 || (mods.length === 1 && mods[0] === 'Shift')) return null;
  return [...mods, key].join('+');
}

/** Human form: `⌥⌘V` on macOS, `Ctrl+Alt+V` elsewhere. */
export function acceleratorLabel(acc: string, mac: boolean): string {
  if (!acc) return '';
  const parts = acc.split('+');
  const key = parts.pop()!;
  if (!mac) return [...parts.map((p) => (p === 'CommandOrControl' || p === 'CmdOrCtrl' || p === 'Control' ? 'Ctrl' : p === 'Super' || p === 'Meta' ? 'Win' : p)), key].join('+');
  const sym: Record<string, string> = { Control: '⌃', Ctrl: '⌃', Alt: '⌥', Option: '⌥', Shift: '⇧', Command: '⌘', Cmd: '⌘', CommandOrControl: '⌘', CmdOrCtrl: '⌘', Super: '⌘', Meta: '⌘' };
  const order = ['⌃', '⌥', '⇧', '⌘'];
  const s = [...new Set(parts.map((p) => sym[p] ?? p))].sort((a, b) => order.indexOf(a) - order.indexOf(b)).join('');
  return `${s}${key}`;
}
