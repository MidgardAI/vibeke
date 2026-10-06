// Per-device UI preferences (theme, sizes, pins, quick replies…), kept in the shell's small
// key-value storage (localStorage in the PWA). Push privacy and notify toggles live on each host
// (`prefs.get/set`), DND is host-wide.

import { ValueStore } from './store';

export type Theme = 'system' | 'light' | 'dark';
export type BeltSize = 's' | 'm' | 'l';

export interface Prefs {
  theme: Theme;
  termFont: number;
  beltSize: BeltSize;
  haptics: boolean;
  zenLandscape: boolean;
  wrap: boolean;
  deviceName: string;
  /** Quick replies per harness id (`*` = default). */
  quickReplies: Record<string, string[]>;
  /** Pinned panes, `<host>/<pane>`. */
  pins: string[];
  /** done_rev seen per `<host>/<run>`. */
  seenDone: Record<string, number>;
  tourDone: boolean;
  /** null = not asked yet. */
  speechConsent: boolean | null;
}

export const DEFAULT_PREFS: Prefs = {
  theme: 'system',
  termFont: 12,
  beltSize: 'm',
  haptics: true,
  zenLandscape: false,
  wrap: true,
  deviceName: '',
  quickReplies: {},
  pins: [],
  seenDone: {},
  tourDone: false,
  speechConsent: null,
};

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);

/** Parse stored JSON leniently: unknown keys dropped, wrong types fall back to defaults. */
export function parsePrefs(raw: string | null): Prefs {
  let v: unknown = null;
  try {
    v = raw ? JSON.parse(raw) : null;
  } catch {
    v = null;
  }
  const p: Prefs = { ...DEFAULT_PREFS };
  if (!isObj(v)) return p;
  if (v.theme === 'light' || v.theme === 'dark' || v.theme === 'system') p.theme = v.theme;
  if (typeof v.termFont === 'number' && v.termFont >= 8 && v.termFont <= 24) p.termFont = v.termFont;
  if (v.beltSize === 's' || v.beltSize === 'm' || v.beltSize === 'l') p.beltSize = v.beltSize;
  for (const k of ['haptics', 'zenLandscape', 'wrap', 'tourDone'] as const) if (typeof v[k] === 'boolean') p[k] = v[k] as boolean;
  if (typeof v.deviceName === 'string') p.deviceName = v.deviceName.slice(0, 64);
  if (typeof v.speechConsent === 'boolean') p.speechConsent = v.speechConsent;
  if (isObj(v.quickReplies)) {
    const q: Record<string, string[]> = {};
    for (const [k, list] of Object.entries(v.quickReplies)) {
      if (Array.isArray(list)) q[k] = list.filter((x): x is string => typeof x === 'string' && x.trim() !== '').slice(0, 20);
    }
    p.quickReplies = q;
  }
  if (Array.isArray(v.pins)) p.pins = v.pins.filter((x): x is string => typeof x === 'string').slice(0, 200);
  if (isObj(v.seenDone)) {
    const s: Record<string, number> = {};
    for (const [k, n] of Object.entries(v.seenDone)) if (typeof n === 'number') s[k] = n;
    p.seenDone = s;
  }
  return p;
}

export interface KV {
  get(key: string): string | null;
  set(key: string, value: string): void;
  remove(key: string): void;
}

const KEY = 'vibeke.prefs';

export class PrefsStore extends ValueStore<Prefs> {
  constructor(private readonly kv: KV) {
    super(parsePrefs(kv.get(KEY)));
  }
  patch(p: Partial<Prefs>): void {
    this.set({ ...this.get(), ...p });
    try {
      this.kv.set(KEY, JSON.stringify(this.get()));
    } catch {
      // storage full or blocked: prefs stay in memory for this session
    }
  }
  togglePin(key: string): void {
    const pins = this.get().pins;
    this.patch({ pins: pins.includes(key) ? pins.filter((k) => k !== key) : [...pins, key] });
  }
  markSeen(runKey: string, doneRev: number): void {
    if (this.get().seenDone[runKey] === doneRev) return;
    this.patch({ seenDone: { ...this.get().seenDone, [runKey]: doneRev } });
  }
}
