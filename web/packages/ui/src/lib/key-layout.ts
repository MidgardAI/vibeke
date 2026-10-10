// The keys board layout: a flat list of keys on a 6-column grid, stored per device in prefs.
// Pure helpers only: presets, validation, row packing, a short shareable code, and the rules
// that decide which keys need a second tap. A key sends 1-4 steps (tokens of the server's key
// grammar, see keys.ts) in one `pane.send_keys` call.

import { isValidKey, keyLabel, press, type Mods } from './keys';

export const GRID_COLS = 6;
export const MAX_STEPS = 4;
export const MAX_KEYS = 60;
export const MAX_LABEL = 8;
export const MAX_NAME = 24;
const MAX_CODE = 6000;
const CODE_PREFIX = 'vk1.';

export type Span = 1 | 2 | 3;

export interface PadKey {
  /** Grammar tokens sent in order, e.g. `['ctrl+c']` or `[':', 'w', 'enter']`. */
  steps: string[];
  /** Shown on the key; empty = derived from the steps. */
  label: string;
  span: Span;
}

export interface KeyLayout {
  name: string;
  keys: PadKey[];
}

const k = (steps: string | string[], label = '', span: Span = 1): PadKey => ({ steps: typeof steps === 'string' ? [steps] : steps, label, span });

export const DEFAULT_KEYS: PadKey[] = [
  k('esc'), k('tab'), k('shift+tab', '⇧⇥'), k('up'), k('ctrl+c', '^C'), k('ctrl+d', '^D'),
  k('space'), k('left'), k('down'), k('right'), k('backspace'), k('enter'),
];

const CLAUDE_KEYS: PadKey[] = [
  k('esc'), k('shift+tab', '⇧⇥'), k('tab'), k('up'), k('down'), k('enter'),
  k('ctrl+o', '^O'), k('ctrl+r', '^R'), k('ctrl+b', '^B'), k('ctrl+c', '^C'),
  k('1'), k('2'), k('3'), k('backspace'),
];

const VIM_KEYS: PadKey[] = [
  k('esc'), k(':'), k('i'), k('u'), k('ctrl+r', '^R'), k('/'),
  k('h'), k('j'), k('k'), k('l'), k(['g', 'g'], 'gg'), k('G'),
  k(['d', 'd'], 'dd'), k([':', 'w', 'enter'], ':w⏎', 2), k([':', 'w', 'q', 'enter'], ':wq⏎', 2), k([':', 'q', '!', 'enter'], ':q!⏎', 2),
];

export type PresetId = 'default' | 'claude' | 'vim';

export const PRESET_IDS: readonly PresetId[] = ['default', 'claude', 'vim'];

const PRESET_NAMES: Record<PresetId, string> = { default: 'Default', claude: 'Claude Code', vim: 'Vim' };

export function preset(id: PresetId): KeyLayout {
  const keys = id === 'claude' ? CLAUDE_KEYS : id === 'vim' ? VIM_KEYS : DEFAULT_KEYS;
  return { name: PRESET_NAMES[id], keys: keys.map(cloneKey) };
}

export const DEFAULT_LAYOUT: KeyLayout = preset('default');

/** F1 to F12 for the function key panel. */
export const FUNCTION_KEYS: PadKey[] = Array.from({ length: 12 }, (_, i) => k(`f${i + 1}`, `F${i + 1}`));

export function cloneKey(key: PadKey): PadKey {
  return { steps: [...key.steps], label: key.label, span: key.span };
}

// ---- validation -------------------------------------------------------------------------------

const isSpan = (n: unknown): n is Span => n === 1 || n === 2 || n === 3;

/** A well-formed key, or null. Labels are trimmed and cut to the limit. */
export function cleanKey(v: unknown): PadKey | null {
  if (!v || typeof v !== 'object') return null;
  const { steps, label, span } = v as Partial<PadKey>;
  if (!Array.isArray(steps) || steps.length < 1 || steps.length > MAX_STEPS) return null;
  if (!steps.every((s) => typeof s === 'string' && isValidKey(s))) return null;
  if (label !== undefined && typeof label !== 'string') return null;
  if (span !== undefined && !isSpan(span)) return null;
  return { steps: [...steps], label: [...(label ?? '').trim()].slice(0, MAX_LABEL).join(''), span: span ?? 1 };
}

/** A well-formed layout (at most MAX_KEYS keys, no invalid key), or null. */
export function cleanLayout(v: unknown): KeyLayout | null {
  if (!v || typeof v !== 'object') return null;
  const { name, keys } = v as Partial<KeyLayout>;
  if (!Array.isArray(keys) || keys.length === 0 || keys.length > MAX_KEYS) return null;
  const out: PadKey[] = [];
  for (const x of keys) {
    const c = cleanKey(x);
    if (!c) return null;
    out.push(c);
  }
  return { name: typeof name === 'string' ? [...name.trim()].slice(0, MAX_NAME).join('') : '', keys: out };
}

// ---- display and safety -----------------------------------------------------------------------

/** What the key shows: its label, else the key names of its steps. */
export const padLabel = (key: PadKey): string => key.label || key.steps.map(keyLabel).join(key.steps.length > 1 ? ' ' : '');

/** Ctrl+D (end of input), Ctrl+Z (suspend), or any sequence that contains Ctrl+C. */
export function isDangerous(key: Pick<PadKey, 'steps'>): boolean {
  if (key.steps.some((s) => s === 'ctrl+d' || s === 'ctrl+z')) return true;
  return key.steps.length > 1 && key.steps.includes('ctrl+c');
}

/** Stable identity of a key within a layout for "armed" state. */
export const padId = (key: PadKey, i: number): string => `${i}:${key.steps.join(' ')}`;

/** Arrow keys without a modifier repeat while held. */
export const isRepeatable = (key: Pick<PadKey, 'steps'>): boolean => key.steps.length === 1 && ['up', 'down', 'left', 'right'].includes(key.steps[0]!);

/**
 * The tokens to send for a tap. A single plain key takes the sticky modifiers (and uses up the
 * "once" ones); a key that carries its own modifier, or a sequence, is sent as written.
 */
export function resolvePad(key: Pick<PadKey, 'steps'>, mods: Mods): { keys: string[]; mods: Mods } {
  if (key.steps.length > 1) return { keys: [...key.steps], mods };
  const base = key.steps[0]!;
  if (/^[a-z]+\+/.test(base)) return { keys: [base], mods };
  const r = press(base, mods);
  return { keys: [r.key], mods: r.mods };
}

// ---- editing ----------------------------------------------------------------------------------

export function moveKey(keys: readonly PadKey[], from: number, to: number): PadKey[] {
  if (from < 0 || from >= keys.length || to < 0 || to >= keys.length || from === to) return [...keys];
  const out = [...keys];
  const [x] = out.splice(from, 1);
  out.splice(to, 0, x!);
  return out;
}

export const removeKey = (keys: readonly PadKey[], i: number): PadKey[] => keys.filter((_, j) => j !== i);

export function addKey(keys: readonly PadKey[], key: PadKey): PadKey[] {
  const c = cleanKey(key);
  return c && keys.length < MAX_KEYS ? [...keys, c] : [...keys];
}

export function setSpan(keys: readonly PadKey[], i: number, span: Span): PadKey[] {
  return keys.map((x, j) => (j === i ? { ...x, span } : x));
}

/** One step from modifiers and a key name or character; null when it is not a valid token. */
export function buildStep(mods: Partial<Record<'ctrl' | 'alt' | 'shift', boolean>>, rawKey: string): string | null {
  const key = rawKey.trim();
  if (!key) return null;
  const base = [...key].length === 1 ? key : key.toLowerCase();
  const on = (['ctrl', 'alt', 'shift'] as const).filter((m) => mods[m]);
  const token = base === '+' && on.length ? `${on.join('+')}++` : [...on, base].join('+');
  return isValidKey(token) ? token : null;
}

// ---- rows -------------------------------------------------------------------------------------

export interface PlacedKey {
  key: PadKey;
  /** Index in the layout (for editing and armed state). */
  index: number;
  /** 1-based grid column where the key starts. */
  start: number;
  span: number;
}

/** Flow keys into rows of GRID_COLS columns. `mirror` flips every row for left-hand use. */
export function packRows(keys: readonly PadKey[], mirror = false): PlacedKey[][] {
  const rows: PlacedKey[][] = [];
  let row: PlacedKey[] = [];
  let used = 0;
  keys.forEach((key, index) => {
    if (used + key.span > GRID_COLS) {
      rows.push(row);
      row = [];
      used = 0;
    }
    row.push({ key, index, start: used + 1, span: key.span });
    used += key.span;
  });
  if (row.length) rows.push(row);
  if (!mirror) return rows;
  return rows.map((r) => r.map((p) => ({ ...p, start: GRID_COLS + 1 - (p.start + p.span - 1) })).reverse());
}

// ---- share code -------------------------------------------------------------------------------

function toB64Url(bytes: Uint8Array): string {
  let s = '';
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

function fromB64Url(s: string): Uint8Array | null {
  if (!/^[A-Za-z0-9_-]*$/.test(s) || s.length % 4 === 1) return null;
  try {
    const bin = atob(s.replace(/-/g, '+').replace(/_/g, '/') + '='.repeat((4 - (s.length % 4)) % 4));
    return Uint8Array.from(bin, (c) => c.charCodeAt(0));
  } catch {
    return null;
  }
}

/** `vk1.` + base64url of `{n, k: [[span, label, ...steps]]}`. */
export function encodeLayout(layout: KeyLayout): string {
  const payload = { n: layout.name, k: layout.keys.map((x) => [x.span, x.label, ...x.steps]) };
  return CODE_PREFIX + toB64Url(new TextEncoder().encode(JSON.stringify(payload)));
}

export type DecodeResult = { ok: true; layout: KeyLayout } | { ok: false; error: 'empty' | 'format' | 'too_long' | 'invalid' };

/** Parse and validate a shared code. Never throws. */
export function decodeLayout(code: string): DecodeResult {
  const text = code.trim();
  if (!text) return { ok: false, error: 'empty' };
  if (text.length > MAX_CODE) return { ok: false, error: 'too_long' };
  if (!text.startsWith(CODE_PREFIX)) return { ok: false, error: 'format' };
  const bytes = fromB64Url(text.slice(CODE_PREFIX.length));
  if (!bytes) return { ok: false, error: 'format' };
  let v: unknown;
  try {
    v = JSON.parse(new TextDecoder('utf-8', { fatal: true }).decode(bytes));
  } catch {
    return { ok: false, error: 'format' };
  }
  if (!v || typeof v !== 'object' || !Array.isArray((v as { k?: unknown }).k)) return { ok: false, error: 'format' };
  const raw = v as { n?: unknown; k: unknown[] };
  const keys: unknown[] = [];
  for (const e of raw.k) {
    if (!Array.isArray(e) || e.length < 3) return { ok: false, error: 'invalid' };
    const [span, label, ...steps] = e as unknown[];
    keys.push({ span, label, steps });
  }
  const layout = cleanLayout({ name: raw.n, keys });
  return layout ? { ok: true, layout } : { ok: false, error: 'invalid' };
}
