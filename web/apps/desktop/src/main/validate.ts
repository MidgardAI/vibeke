// Hand-written validators for every IPC argument (spec 16 §16.1). Electron-free so they are unit
// tested; ipc.ts applies them before any handler logic runs.

import { isAbsolute } from 'node:path';
import { RENDERER_METHODS, type DesktopSettings, type RendererMethod, type RendererSettingsPatch, type Theme, type WindowOp } from '../shared/contract';

export class IpcValidationError extends Error {
  constructor(message: string) {
    super(`invalid IPC: ${message}`);
    this.name = 'IpcValidationError';
  }
}

const fail = (m: string): never => {
  throw new IpcValidationError(m);
};

/** The renderer origin the app trusts: the bundled app (`app://vibeke`) or the dev server. */
export function isTrustedUrl(url: string | undefined | null, trusted: readonly string[]): boolean {
  if (!url) return false;
  let u: URL;
  try {
    u = new URL(url);
  } catch {
    return false;
  }
  return trusted.some((t) => {
    try {
      const o = new URL(t);
      return u.protocol === o.protocol && u.host === o.host;
    } catch {
      return false;
    }
  });
}

/** Host ids are base32 (26 chars); allow a little slack for tests and future formats. */
export function hostId(v: unknown): string {
  return typeof v === 'string' && /^[A-Za-z0-9_-]{1,64}$/.test(v) ? v : fail('host id');
}

/** Pane/interaction ids and handles. */
export function entityId(v: unknown, what = 'id'): string {
  return typeof v === 'string' && v.length > 0 && v.length <= 200 && !/[\0-\x1f]/.test(v) ? v : fail(what);
}

const METHODS = new Set<string>(RENDERER_METHODS);
export function method(v: unknown): RendererMethod {
  return typeof v === 'string' && METHODS.has(v) ? (v as RendererMethod) : fail('method');
}

const isPlain = (v: unknown): v is Record<string, unknown> => {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
};

/** JSON-shaped values only (structured clone already removed functions), bounded size/depth. */
function jsonSize(v: unknown, depth: number, budget: { left: number }): void {
  if (depth > 32) fail('params too deep');
  if (v === null || typeof v === 'boolean') return void (budget.left -= 4);
  if (typeof v === 'number') return Number.isFinite(v) ? void (budget.left -= 8) : fail('non-finite number');
  if (typeof v === 'string') return void (budget.left -= v.length + 2);
  if (Array.isArray(v)) {
    budget.left -= 2;
    for (const x of v) jsonSize(x, depth + 1, budget);
  } else if (isPlain(v)) {
    budget.left -= 2;
    for (const [k, x] of Object.entries(v)) {
      budget.left -= k.length + 3;
      jsonSize(x, depth + 1, budget);
    }
  } else fail('params must be JSON');
  if (budget.left < 0) fail('params too large');
}

/** 12 MiB: attachments are ≤ 8 MiB (base64 ≈ 10.7 MiB); handoff chunks ≤ 4 MiB. */
export const MAX_PARAMS = 12 * 1024 * 1024;

export function params(v: unknown): Record<string, unknown> {
  if (v === undefined) return {};
  if (!isPlain(v)) fail('params must be an object');
  jsonSize(v, 0, { left: MAX_PARAMS });
  return v as Record<string, unknown>;
}

export function requestOpts(v: unknown): { timeoutMs?: number } {
  if (v === undefined || v === null) return {};
  if (!isPlain(v)) fail('opts');
  const o = v as Record<string, unknown>;
  for (const k of Object.keys(o)) if (k !== 'timeoutMs') fail(`opts.${k}`);
  if (o.timeoutMs === undefined) return {};
  const n = o.timeoutMs;
  return typeof n === 'number' && Number.isInteger(n) && n >= 100 && n <= 600_000 ? { timeoutMs: n } : fail('opts.timeoutMs');
}

export function shortText(v: unknown, what: string, max = 200): string {
  return typeof v === 'string' && v.length <= max && !/[\0]/.test(v) ? v : fail(what);
}

/** A pairing link as pasted/opened: bounded, printable. Parsed by core afterwards. */
export function linkText(v: unknown): string {
  return typeof v === 'string' && v.length > 0 && v.length <= 8192 && !/[\0-\x08\x0e-\x1f]/.test(v) ? v.trim() : fail('link');
}

export function pairToken(v: unknown): string {
  return typeof v === 'string' && /^[A-Za-z0-9-]{8,64}$/.test(v) ? v : fail('pair token');
}

/** Only http(s) URLs leave the app (spec 16 §9.3 rendering safety). */
export function externalUrl(v: unknown): string {
  if (typeof v !== 'string' || v.length > 4096) return fail('url');
  let u: URL;
  try {
    u = new URL(v);
  } catch {
    return fail('url');
  }
  if (u.protocol !== 'https:' && u.protocol !== 'http:') fail('url scheme');
  if (u.username || u.password) fail('url credentials');
  return u.toString();
}

/** In-app hash routes (`#/inbox`, `#/h/<host>/p/<pane>`…). */
export function hashRoute(v: unknown): string {
  return typeof v === 'string' && v.startsWith('#/') && v.length <= 8192 && !/[\0-\x1f\s]/.test(v) ? v : fail('hash');
}

export function windowOp(v: unknown): WindowOp {
  if (!isPlain(v)) return fail('window op');
  switch (v.op) {
    case 'pop-out':
      return { op: 'pop-out', host: hostId(v.host), pane: entityId(v.pane, 'pane') };
    case 'open-main':
      return { op: 'open-main', hash: hashRoute(v.hash) };
    case 'close':
      return { op: 'close' };
    case 'quick':
      return { op: 'quick' };
    default:
      return fail('window op');
  }
}

/** `vk:host.events` on/off flag. */
export function flag(v: unknown, what: string): boolean {
  return typeof v === 'boolean' ? v : fail(what);
}

export function theme(v: unknown): Theme {
  return v === 'system' || v === 'light' || v === 'dark' ? v : fail('theme');
}

const MODIFIERS = new Set(['Command', 'Cmd', 'Control', 'Ctrl', 'CommandOrControl', 'CmdOrCtrl', 'Alt', 'Option', 'AltGr', 'Shift', 'Super', 'Meta']);
const KEYS = /^([A-Z0-9]|F([1-9]|1[0-9]|2[0-4])|Plus|Space|Tab|Backspace|Delete|Insert|Return|Enter|Up|Down|Left|Right|Home|End|PageUp|PageDown|Escape|Esc|[`~!@#$%^&*()\-_=[\]{};:'",.<>/?\\|])$/;

/** An Electron accelerator with at least one modifier (a bare key would swallow typing). */
export function accelerator(v: unknown): string {
  if (v === '') return '';
  if (typeof v !== 'string' || v.length > 64) return fail('shortcut');
  const parts = v.split('+');
  if (parts.length < 2) fail('shortcut needs a modifier');
  const key = parts.pop()!;
  for (const m of parts) if (!MODIFIERS.has(m)) fail(`shortcut modifier ${m}`);
  if (new Set(parts).size !== parts.length) fail('shortcut repeats a modifier');
  if (!KEYS.test(key)) fail(`shortcut key ${key}`);
  return v;
}

/**
 * A partial settings update from a renderer. Only presentation/behaviour toggles: the `vibeke`
 * executable is chosen through the main-process picker and the update feed is packaged, so a
 * compromised renderer can neither pick what gets executed nor where updates come from.
 */
export function settingsPatch(v: unknown): RendererSettingsPatch {
  if (!isPlain(v)) return fail('settings');
  const out: RendererSettingsPatch = {};
  for (const [k, x] of Object.entries(v)) {
    switch (k) {
      case 'shortcut':
        out.shortcut = accelerator(x);
        break;
      case 'openAtLogin':
      case 'showDock':
      case 'automaticUpdates':
      case 'notifications':
        if (typeof x !== 'boolean') fail(`settings.${k}`);
        out[k] = x as boolean;
        break;
      default:
        fail(`settings.${k}`);
    }
  }
  return out;
}

/**
 * The folder the native folder picker opens in: absent, or an absolute path. It only seeds the
 * dialog (the user still chooses), but stays bounded and free of control characters.
 */
export function defaultDirectory(v: unknown): string | undefined {
  if (v === undefined || v === null || v === '') return undefined;
  return typeof v === 'string' && v.length <= 4096 && isAbsolute(v) && !/[\0-\x1f]/.test(v) ? v : fail('default path');
}

/** An absolute executable path as stored by main after the picker validated it. */
export function storedExecutablePath(v: unknown): string {
  return v === '' || (typeof v === 'string' && isAbsolute(v) && v.length <= 1024 && !/[\0\n]/.test(v)) ? (v as string) : fail('settings.vibekePath');
}

/** One key of settings.json (written by main; a hand edit must not break startup). */
export function storedSetting(k: string, v: unknown): Partial<DesktopSettings> {
  if (k === 'vibekePath') return { vibekePath: storedExecutablePath(v) };
  return settingsPatch({ [k]: v });
}
