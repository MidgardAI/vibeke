// Keyboard-first navigation (spec 16 §16.2), shared by the desktop app and the PWA (iPad with a
// hardware keyboard): pure key → action mapping, so it is testable without a DOM.

import type { Tab } from '../router';

export type ShortcutAction =
  | { type: 'next' }
  | { type: 'prev' }
  | { type: 'allow' }
  | { type: 'deny' }
  | { type: 'allowAlways' }
  | { type: 'open' }
  | { type: 'back' }
  | { type: 'tab'; tab: Tab }
  | { type: 'find' }
  | { type: 'palette' }
  | { type: 'help' };

export interface KeyLike {
  key: string;
  metaKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  shiftKey: boolean;
  /** IME composition in progress: never a shortcut. */
  isComposing?: boolean;
  /** Auto-repeat from a held key. */
  repeat?: boolean;
}

export interface KeyContext {
  /** Cmd (macOS/iPadOS) vs Ctrl (elsewhere) as the command modifier. */
  mac: boolean;
  /** Focus is in a text field / contenteditable. */
  typing: boolean;
  /** A modal sheet or dialog is open (it handles its own keys). */
  dialog: boolean;
  /** Focus is on a button/link, where Enter/Space already activate it. */
  onControl: boolean;
}

export const TAB_ORDER: readonly Tab[] = ['inbox', 'panes', 'focus', 'changes'];

/** Actions that answer interactions: never from a held (auto-repeating) key. */
const MUTATING = new Set<ShortcutAction['type']>(['allow', 'deny', 'allowAlways', 'open']);

/** Map a keydown to an app action, or null to let it through untouched. */
export function shortcutFor(e: KeyLike, ctx: KeyContext): ShortcutAction | null {
  const a = mapKey(e, ctx);
  return a && e.repeat && MUTATING.has(a.type) ? null : a;
}

function mapKey(e: KeyLike, ctx: KeyContext): ShortcutAction | null {
  if (e.isComposing) return null;
  const mod = ctx.mac ? e.metaKey && !e.ctrlKey : e.ctrlKey && !e.metaKey;
  if (mod && !e.altKey) {
    const k = e.key.toLowerCase();
    if (k === 'k' && !e.shiftKey) return { type: 'palette' };
    const n = Number(e.key);
    if (!e.shiftKey && Number.isInteger(n) && n >= 1 && n <= TAB_ORDER.length) return { type: 'tab', tab: TAB_ORDER[n - 1]! };
    return null;
  }
  if (ctx.typing || ctx.dialog) return null;
  if (e.metaKey || e.ctrlKey || e.altKey) return null;
  switch (e.key) {
    case 'j':
      return { type: 'next' };
    case 'k':
      return { type: 'prev' };
    case 'a':
      return { type: 'allow' };
    case 'A':
      return { type: 'allowAlways' };
    case 'd':
      return { type: 'deny' };
    case 'Enter':
      return ctx.onControl ? null : { type: 'open' };
    case 'Escape':
      return { type: 'back' };
    case '/':
      return { type: 'find' };
    case '?':
      return { type: 'help' };
    default:
      return null;
  }
}

/** Display form of a shortcut for the cheat sheet / palette, e.g. `⌘K` or `Ctrl+K`. */
export function keyLabel(mac: boolean, combo: string): string {
  if (!combo.startsWith('mod+')) return combo;
  const rest = combo.slice(4).toUpperCase();
  return mac ? `⌘${rest}` : `Ctrl+${rest}`;
}

/** Shortcuts listed in the `?` cheat sheet (mod = ⌘ on Apple platforms, Ctrl elsewhere). */
export const SHORTCUTS: readonly { keys: string[]; what: string }[] = [
  { keys: ['mod+k'], what: 'palette' },
  { keys: ['j', 'k'], what: 'move' },
  { keys: ['a'], what: 'allow' },
  { keys: ['d'], what: 'deny' },
  { keys: ['A'], what: 'allowAlways' },
  { keys: ['Enter'], what: 'open' },
  { keys: ['Esc'], what: 'back' },
  { keys: ['mod+1', 'mod+2', 'mod+3', 'mod+4'], what: 'tabs' },
  { keys: ['/'], what: 'find' },
  { keys: ['?'], what: 'help' },
];

/** Subsequence fuzzy score (higher is better), or -1 when `q` does not match `text`. */
export function fuzzyScore(q: string, text: string): number {
  const query = q.trim().toLowerCase();
  if (!query) return 0;
  const s = text.toLowerCase();
  const direct = s.indexOf(query);
  if (direct >= 0) return 1000 - direct * 2 - (s.length - query.length) * 0.1 + (direct === 0 || /[\s·/:_-]/.test(s[direct - 1] ?? '') ? 50 : 0);
  let score = 0;
  let pos = -1;
  let run = 0;
  for (const ch of query) {
    if (ch === ' ') continue;
    const i = s.indexOf(ch, pos + 1);
    if (i < 0) return -1;
    run = i === pos + 1 ? run + 1 : 0;
    score += 10 + run * 5 - Math.min(i - pos - 1, 10);
    if (i === 0 || /[\s·/:_-]/.test(s[i - 1] ?? '')) score += 8;
    pos = i;
  }
  return score;
}
