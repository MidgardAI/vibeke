// Small JSON files in the user-data dir (desktop settings, window bounds), written atomically.
// Not secret: keys and host records live in the encrypted vault.

import { mkdirSync, readFileSync, renameSync, writeFileSync } from 'node:fs';
import { dirname } from 'node:path';
import { DEFAULT_SETTINGS, type DesktopSettings } from '../shared/contract';
import { storedSetting } from './validate';

export function readJson<T>(file: string, fallback: T): T {
  try {
    return JSON.parse(readFileSync(file, 'utf8')) as T;
  } catch {
    return fallback;
  }
}

export function writeJson(file: string, value: unknown): void {
  mkdirSync(dirname(file), { recursive: true, mode: 0o700 });
  const tmp = `${file}.${process.pid}.tmp`;
  writeFileSync(tmp, JSON.stringify(value, null, 2), { mode: 0o600 });
  renameSync(tmp, file);
}

/** Load settings, dropping anything invalid or retired (e.g. the old `updateFeed`). */
export function loadSettings(file: string): DesktopSettings {
  const raw = readJson<Record<string, unknown>>(file, {});
  const out: DesktopSettings = { ...DEFAULT_SETTINGS };
  for (const [k, v] of Object.entries(raw ?? {})) {
    try {
      Object.assign(out, storedSetting(k, v));
    } catch {
      // ignore this key
    }
  }
  return out;
}

export interface Bounds {
  x: number;
  y: number;
  width: number;
  height: number;
  maximized?: boolean;
}

export interface Rect {
  x: number;
  y: number;
  width: number;
  height: number;
}

/**
 * Restore saved bounds only if they are sane and still mostly on a connected display (monitors
 * come and go); otherwise null (the caller centres the window).
 */
export function restoreBounds(saved: unknown, displays: readonly Rect[], min: { width: number; height: number }): Bounds | null {
  const b = saved as Partial<Bounds> | null;
  if (!b || ![b.x, b.y, b.width, b.height].every((n) => typeof n === 'number' && Number.isFinite(n))) return null;
  const w = Math.max(min.width, Math.round(b.width!));
  const h = Math.max(min.height, Math.round(b.height!));
  const x = Math.round(b.x!);
  const y = Math.round(b.y!);
  const visible = displays.some((d) => {
    const ix = Math.max(0, Math.min(x + w, d.x + d.width) - Math.max(x, d.x));
    const iy = Math.max(0, Math.min(y + h, d.y + d.height) - Math.max(y, d.y));
    return ix * iy >= Math.min(w * h, 200 * 120) * 0.5 && y >= d.y - 10;
  });
  if (!visible) return null;
  return { x, y, width: w, height: h, maximized: b.maximized === true };
}

/** Popover position: centred under the tray icon, kept inside the work area. */
export function popoverPosition(tray: Rect | null, size: { width: number; height: number }, work: Rect, platform: string): { x: number; y: number } {
  if (!tray || tray.width === 0) {
    // No tray geometry (Linux/global shortcut): top-right corner of the work area.
    return { x: work.x + work.width - size.width - 12, y: platform === 'darwin' ? work.y + 6 : work.y + 12 };
  }
  let x = Math.round(tray.x + tray.width / 2 - size.width / 2);
  // Tray at the bottom (Windows taskbar) → open upwards.
  const below = tray.y < work.y + work.height / 2;
  let y = below ? Math.round(tray.y + tray.height + 4) : Math.round(tray.y - size.height - 4);
  x = Math.max(work.x + 6, Math.min(x, work.x + work.width - size.width - 6));
  y = Math.max(work.y + 4, Math.min(y, work.y + work.height - size.height - 4));
  return { x, y };
}
