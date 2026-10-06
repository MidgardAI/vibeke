// The Keys belt (spec 16 §9.1 Action belt): sticky modifiers (off → once → locked), key names in
// the server's key grammar (crates/vk-term/src/keygrammar.rs: `ctrl+c`, `shift+tab`, `esc`, `up`),
// and chord mode (queue keys as chips, send them in one `pane.send_keys`).

export type Modifier = 'ctrl' | 'alt' | 'shift';
export type ModState = 'off' | 'once' | 'locked';
export type Mods = Record<Modifier, ModState>;

export const NO_MODS: Mods = { ctrl: 'off', alt: 'off', shift: 'off' };

/** Tap cycles a sticky modifier: off → once → locked → off. */
export const cycleMod = (s: ModState): ModState => (s === 'off' ? 'once' : s === 'once' ? 'locked' : 'off');

/** Canonical modifier order of the grammar's formatter. */
const ORDER: Modifier[] = ['ctrl', 'alt', 'shift'];

/** Key names valid in the grammar (base keys only; modifiers are prefixed with `+`). */
export const NAMED_KEYS = new Set([
  'enter', 'tab', 'esc', 'space', 'backspace', 'delete', 'insert', 'home', 'end', 'pageup', 'pagedown',
  'up', 'down', 'left', 'right',
]);

/** Build one chord, e.g. (`c`, ctrl) → `ctrl+c`. Shift on a single letter is kept as a modifier. */
export function chord(base: string, mods: Mods): string {
  const on = ORDER.filter((m) => mods[m] !== 'off');
  if (base === '+' && on.length) return `${on.join('+')}++`;
  return [...on, base].join('+');
}

/** Press a key with the current modifiers: returns the chord and the modifiers after the press. */
export function press(base: string, mods: Mods): { key: string; mods: Mods } {
  const key = chord(base, mods);
  const next: Mods = { ...mods };
  for (const m of ORDER) if (next[m] === 'once') next[m] = 'off';
  return { key, mods: next };
}

/** Validate a key token against the grammar subset the belt can produce. */
export function isValidKey(k: string): boolean {
  if (!k) return false;
  const parts = k === '+' ? ['+'] : k.endsWith('++') ? [...k.slice(0, -2).split('+'), '+'] : k.split('+');
  const base = parts.pop()!;
  for (const m of parts) if (!['ctrl', 'alt', 'shift'].includes(m)) return false;
  if ([...base].length === 1) return true;
  return NAMED_KEYS.has(base) || /^f([1-9]|1\d|2[0-4])$/.test(base);
}

/** A chord queue (chord mode). */
export interface KeyQueue {
  keys: string[];
}

export const queueAdd = (q: KeyQueue, k: string): KeyQueue => ({ keys: [...q.keys, k] });
export const queueRemoveAt = (q: KeyQueue, i: number): KeyQueue => ({ keys: q.keys.filter((_, j) => j !== i) });

/** Human label for a chord chip. */
export function keyLabel(k: string): string {
  const map: Record<string, string> = {
    enter: '⏎', tab: '⇥', esc: 'Esc', space: '␣', backspace: '⌫', up: '↑', down: '↓', left: '←', right: '→',
    ctrl: '⌃', alt: '⌥', shift: '⇧', delete: 'Del', home: 'Home', end: 'End', pageup: 'PgUp', pagedown: 'PgDn',
  };
  if (k.endsWith('++')) return `${k.slice(0, -2).split('+').map((p) => map[p] ?? p).join('')}+`;
  return k
    .split('+')
    .map((p) => map[p] ?? (p.length === 1 ? p.toUpperCase() : p))
    .join('');
}
