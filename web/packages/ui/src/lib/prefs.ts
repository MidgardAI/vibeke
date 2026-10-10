// Per-device UI preferences (theme, sizes, pins, quick replies…), kept in the shell's small
// key-value storage (localStorage in the PWA). Push privacy and notify toggles live on each host
// (`prefs.get/set`), DND is host-wide.

import { MAX_VIEW_OVERRIDES, isAgentView, withViewOverride, type AgentView } from './agent-view';
import { isLanguage, type LanguagePref } from '../i18n';
import { ValueStore } from './store';

export type Theme = 'system' | 'light' | 'dark';
export type BeltSize = 's' | 'm' | 'l';

export interface Prefs {
  theme: Theme;
  termFont: number;
  beltSize: BeltSize;
  /** Interface language; `system` follows the browser. */
  language: LanguagePref;
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
  /** Wide layout: the right (changes) panel is open on workspace routes. */
  panelOpen: boolean;
  /** Wide layout: right panel width in px. */
  panelWidth: number;
  /** Wide layout: the sidebar is hidden (mod+backslash). */
  sidebarHidden: boolean;
  /** Collapsed sidebar sections (`pinned`, `needs`, `review`, `working`, `done`, `idle`). */
  collapsed: string[];
  /** Sidebar shows the Done and Idle groups. */
  showDone: boolean;
  /** Sidebar host filter (host id), null = all hosts. */
  hostFilter: string | null;
  /** How agents are shown by default: the structured conversation or the agent's own terminal. */
  agentView: AgentView;
  /** Per-workspace overrides of `agentView`, keyed `<host>/<workspace>`. */
  agentViews: Record<string, AgentView>;
}

export const PANEL_MIN = 320;
export const PANEL_MAX = 760;

export const DEFAULT_PREFS: Prefs = {
  theme: 'dark',
  termFont: 12,
  beltSize: 'm',
  language: 'system',
  haptics: true,
  zenLandscape: false,
  wrap: true,
  deviceName: '',
  quickReplies: {},
  pins: [],
  seenDone: {},
  tourDone: false,
  speechConsent: null,
  panelOpen: true,
  panelWidth: 420,
  sidebarHidden: false,
  collapsed: [],
  showDone: true,
  hostFilter: null,
  agentView: 'conversation',
  agentViews: {},
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
  if (v.language === 'system' || isLanguage(v.language)) p.language = v.language;
  if (v.beltSize === 's' || v.beltSize === 'm' || v.beltSize === 'l') p.beltSize = v.beltSize;
  for (const k of ['haptics', 'zenLandscape', 'wrap', 'tourDone', 'panelOpen', 'sidebarHidden', 'showDone'] as const) if (typeof v[k] === 'boolean') p[k] = v[k] as boolean;
  if (typeof v.deviceName === 'string') p.deviceName = v.deviceName.slice(0, 64);
  if (typeof v.speechConsent === 'boolean') p.speechConsent = v.speechConsent;
  if (isObj(v.quickReplies)) {
    const q: Record<string, string[]> = {};
    for (const [k, list] of Object.entries(v.quickReplies)) {
      if (Array.isArray(list)) q[k] = list.filter((x): x is string => typeof x === 'string' && x.trim() !== '').slice(0, 20);
    }
    p.quickReplies = q;
  }
  if (typeof v.panelWidth === 'number' && Number.isFinite(v.panelWidth)) p.panelWidth = Math.round(Math.min(PANEL_MAX, Math.max(PANEL_MIN, v.panelWidth)));
  if (Array.isArray(v.collapsed)) p.collapsed = v.collapsed.filter((x): x is string => typeof x === 'string').slice(0, 20);
  if (typeof v.hostFilter === 'string') p.hostFilter = v.hostFilter;
  if (isAgentView(v.agentView)) p.agentView = v.agentView;
  if (isObj(v.agentViews)) {
    const m: Record<string, AgentView> = {};
    for (const [k, view] of Object.entries(v.agentViews).slice(-MAX_VIEW_OVERRIDES)) if (isAgentView(view)) m[k] = view;
    p.agentViews = m;
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
  /** Changes made elsewhere (another window/tab of the same origin). Returns an unsubscribe. */
  watch?(key: string, cb: (value: string | null) => void): () => void;
}

const KEY = 'vibeke.prefs';

export class PrefsStore extends ValueStore<Prefs> {
  constructor(private readonly kv: KV) {
    super(parsePrefs(kv.get(KEY)));
    // Another window changed prefs (theme, pins…): adopt them without writing back.
    kv.watch?.(KEY, (raw) => this.set(parsePrefs(raw)));
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
  toggleCollapsed(id: string): void {
    const c = this.get().collapsed;
    this.patch({ collapsed: c.includes(id) ? c.filter((x) => x !== id) : [...c, id] });
  }
  /** Show agents of `host/workspace` as `view` on this device; null = back to the default. */
  setWorkspaceView(host: string, workspace: string, view: AgentView | null): void {
    this.patch({ agentViews: withViewOverride(this.get().agentViews, host, workspace, view) });
  }
  markSeen(runKey: string, doneRev: number): void {
    if (this.get().seenDone[runKey] === doneRev) return;
    this.patch({ seenDone: { ...this.get().seenDone, [runKey]: doneRev } });
  }
}
