// Phone preview: the pure parts. Pointer events become touch events in the page's viewport,
// batched so one `browser.input` call carries everything that happened in one animation frame.

import type { BrowserInputEvent } from '@vibeke/core';
import type { Size } from './screencast';

/** The host accepts at most 64 events per `browser.input` call. */
export const MAX_BATCH = 64;

type Point = { id: number; x: number; y: number };

/** A client position (CSS px in the page) as a point in the viewport; the box may differ in size from the viewport. */
export function toViewport(client: { x: number; y: number }, box: { left: number; top: number; width: number; height: number }, viewport: Size): { x: number; y: number } {
  const kx = box.width > 0 ? viewport.width / box.width : 1;
  const ky = box.height > 0 ? viewport.height / box.height : 1;
  const x = Math.min(Math.max(0, (client.x - box.left) * kx), Math.max(0, viewport.width - 1));
  const y = Math.min(Math.max(0, (client.y - box.top) * ky), Math.max(0, viewport.height - 1));
  return { x: Math.round(x * 10) / 10, y: Math.round(y * 10) / 10 };
}

/**
 * Collects input events. A touch event lists every finger that is down after the change (the
 * lowest free touch id goes to a new pointer). Consecutive moves merge into the newest one,
 * consecutive wheel events add up, and only the last resize counts.
 */
export class InputBatcher {
  private queue: BrowserInputEvent[] = [];
  private active = new Map<number, number>(); // pointer id -> touch id
  private pos = new Map<number, Point>(); // pointer id -> last point

  get pending(): boolean {
    return this.queue.length > 0;
  }
  get touching(): boolean {
    return this.active.size > 0;
  }

  private points(): Point[] {
    return [...this.pos.values()].sort((a, b) => a.id - b.id);
  }

  private freeId(): number {
    const used = new Set(this.active.values());
    let id = 0;
    while (used.has(id)) id++;
    return id;
  }

  down(pointer: number, x: number, y: number): void {
    if (this.active.has(pointer)) return;
    const id = this.freeId();
    this.active.set(pointer, id);
    this.pos.set(pointer, { id, x, y });
    this.queue.push({ type: 'touch', phase: 'start', points: this.points() });
  }

  move(pointer: number, x: number, y: number): void {
    const p = this.pos.get(pointer);
    if (!p || (p.x === x && p.y === y)) return;
    this.pos.set(pointer, { id: p.id, x, y });
    const last = this.queue[this.queue.length - 1];
    const ev: BrowserInputEvent = { type: 'touch', phase: 'move', points: this.points() };
    if (last && last.type === 'touch' && last.phase === 'move') this.queue[this.queue.length - 1] = ev;
    else this.queue.push(ev);
  }

  /** The finger lifts (`cancel` for an interrupted gesture). The event lists the fingers still down. */
  up(pointer: number, cancel = false): void {
    if (!this.active.has(pointer)) return;
    this.active.delete(pointer);
    this.pos.delete(pointer);
    this.queue.push({ type: 'touch', phase: cancel ? 'cancel' : 'end', points: this.points() });
  }

  wheel(x: number, y: number, dx: number, dy: number): void {
    const last = this.queue[this.queue.length - 1];
    if (last && last.type === 'wheel') this.queue[this.queue.length - 1] = { type: 'wheel', x, y, dx: last.dx + dx, dy: last.dy + dy };
    else this.queue.push({ type: 'wheel', x, y, dx, dy });
  }

  /** Any other event (text, key, history). */
  push(ev: BrowserInputEvent): void {
    if (ev.type === 'resize') {
      const i = this.queue.findIndex((e) => e.type === 'resize');
      if (i >= 0) this.queue.splice(i, 1);
    }
    this.queue.push(ev);
  }

  /** The next batch (at most `MAX_BATCH`); the rest stays queued. */
  take(): BrowserInputEvent[] {
    return this.queue.splice(0, MAX_BATCH);
  }

  clear(): void {
    this.queue = [];
    this.active.clear();
    this.pos.clear();
  }
}

/** What a text input reports (`beforeinput`) as an event for the page, or null when nothing should be sent. */
export function inputToEvent(inputType: string, data: string | null): BrowserInputEvent | null {
  switch (inputType) {
    case 'insertText':
    case 'insertFromPaste':
    case 'insertReplacementText':
    case 'insertFromComposition':
      return data ? { type: 'text', text: data } : null;
    case 'insertLineBreak':
    case 'insertParagraph':
      return { type: 'key', key: 'Enter' };
    case 'deleteContentBackward':
    case 'deleteWordBackward':
    case 'deleteSoftLineBackward':
      return { type: 'key', key: 'Backspace' };
    case 'deleteContentForward':
      return { type: 'key', key: 'Delete' };
    default:
      return null;
  }
}

/** Named keys a key press sends as `key` events (other keys arrive as text through `beforeinput`). */
const NAMED_KEYS = new Set(['Enter', 'Backspace', 'Delete', 'Tab', 'Escape', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight', 'Home', 'End', 'PageUp', 'PageDown']);
export const namedKey = (key: string): string | null => (NAMED_KEYS.has(key) ? key : null);

/**
 * The size the page may use: the box, cut to the visual viewport when it is smaller (an open
 * keyboard). `top` is the box's top edge in layout coordinates.
 */
export function usableSize(box: { top: number; width: number; height: number }, vv: { height: number; offsetTop: number } | null): Size {
  let height = box.height;
  if (vv) height = Math.min(height, vv.height + vv.offsetTop - box.top);
  return { width: Math.max(0, Math.floor(box.width)), height: Math.max(0, Math.floor(height)) };
}

/** True when two sizes differ by more than a pixel or two (ignores layout jitter). */
export const sizeChanged = (a: Size, b: Size): boolean => Math.abs(a.width - b.width) > 1 || Math.abs(a.height - b.height) > 1;

/**
 * `localhost` in a browser on this device is not the preview machine's localhost, so such URLs
 * must not be offered as "open in browser": localhost, *.localhost, 127.0.0.0/8, [::1], 0.0.0.0.
 */
export function isLoopbackUrl(url: string): boolean {
  let host: string;
  try {
    host = new URL(url).hostname.toLowerCase();
  } catch {
    return false;
  }
  if (host.endsWith('.')) host = host.slice(0, -1);
  if (host === 'localhost' || host.endsWith('.localhost')) return true;
  if (host === '[::1]' || host === '::1' || host === '0.0.0.0' || host === '[::]') return true;
  const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/.exec(host);
  return !!m && Number(m[1]) === 127;
}
