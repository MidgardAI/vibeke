// Host events forwarded from the main process's connections to the windows that asked for them
// (`vk:host-event`). Only the event types the UI reacts to cross the bridge, as plain JSON with a
// bounded size; the renderer validates every payload again before handing it to the UI.
// Electron-free so both sides (and the unit tests) share it.

import type { AppEvent } from '@vibeke/core';

/** Event type prefixes (and exact types) the UI listens to. */
const PREFIXES = ['agent.', 'interaction.', 'task.', 'preview.', 'tab.', 'pane.', 'handoff.'] as const;
const EXACT = new Set(['notification.created']);

/** Payload of `vk:host-event`. */
export interface HostEventPayload {
  hostId: string;
  event: AppEvent;
}

/** Larger events are dropped (the UI refetches what it needs; events are hints). */
export const MAX_EVENT_BYTES = 64 * 1024;

const TYPE_RE = /^[a-z][a-z0-9_]*(\.[a-z0-9_]+)+$/;
const HOST_RE = /^[A-Za-z0-9_-]{1,64}$/;

export function isForwardedEventType(type: unknown): type is string {
  if (typeof type !== 'string' || type.length > 64 || !TYPE_RE.test(type)) return false;
  return EXACT.has(type) || PREFIXES.some((p) => type.startsWith(p));
}

const isPlain = (v: unknown): v is Record<string, unknown> => {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
};

/** `subject` keeps string ids only (run, pane, interaction, task…). */
function subject(v: unknown): Record<string, string> | null {
  if (!isPlain(v)) return null;
  const out: Record<string, string> = {};
  for (const [k, x] of Object.entries(v)) {
    if (k.length > 32) return null;
    if (x === undefined || x === null) continue;
    if (typeof x !== 'string' || x.length > 200) return null;
    out[k] = x;
  }
  return out;
}

/**
 * The event as it may cross to a renderer, or null when its type is not forwarded or it is
 * malformed / too large. Used by main before sending and by the renderer on receipt.
 */
export function forwardableEvent(e: unknown): AppEvent | null {
  if (!isPlain(e)) return null;
  if (!isForwardedEventType(e.type)) return null;
  if (typeof e.seq !== 'number' || !Number.isSafeInteger(e.seq) || e.seq < 0) return null;
  const ts = typeof e.ts === 'number' && Number.isFinite(e.ts) ? e.ts : 0;
  const s = subject(e.subject ?? {});
  if (!s) return null;
  const data = e.data === undefined || e.data === null ? {} : e.data;
  if (!isPlain(data)) return null;
  let size: number;
  try {
    size = JSON.stringify(data).length;
  } catch {
    return null;
  }
  if (size > MAX_EVENT_BYTES) return null;
  return { seq: e.seq, ts, type: e.type, subject: s, data: JSON.parse(JSON.stringify(data)) as Record<string, unknown> };
}

/** Renderer side: a `vk:host-event` payload, validated (null = drop it). */
export function parseHostEvent(p: unknown): HostEventPayload | null {
  if (!isPlain(p) || typeof p.hostId !== 'string' || !HOST_RE.test(p.hostId)) return null;
  const event = forwardableEvent(p.event);
  return event ? { hostId: p.hostId, event } : null;
}

/**
 * Main side: which windows asked for which hosts' events. Keyed by an opaque window key
 * (webContents); a reload drops the window's subscriptions (`clear`).
 */
export class EventSubscriptions<K extends object> {
  private subs = new WeakMap<K, Set<string>>();

  set(win: K, hostId: string, on: boolean): void {
    let s = this.subs.get(win);
    if (!s) {
      if (!on) return;
      this.subs.set(win, (s = new Set()));
    }
    if (on) s.add(hostId);
    else s.delete(hostId);
  }

  clear(win: K): void {
    this.subs.delete(win);
  }

  wants(win: K, hostId: string): boolean {
    return this.subs.get(win)?.has(hostId) ?? false;
  }
}
