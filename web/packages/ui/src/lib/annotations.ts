// Page annotations on the app side: the pure helpers (pick gestures, note labels, thumbnails) and
// `useAnnotations`, which keeps one draft of notes in step with the host that owns the page.
// The draft lives on the host; this device only reads it and sends edits.

import { useCallback, useEffect, useRef, useState } from 'react';
import type { AnnotationDraft, AnnotationNote, AnnotationPick, AnnotationState } from '@vibeke/core';
import { useApp, useConnOf } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from './answer';
import type { Size } from './screencast';

/** A press that moves less than this (viewport px) is a tap, not a region. */
export const DRAG_MIN_PX = 8;
/** A touch held this long before moving starts a region instead of a tap. */
export const LONG_PRESS_MS = 350;
const POLL_MS = 3000;

export type Pt = { x: number; y: number };

/** Params of `browser.annotation_pick` for a tap (`end` only) or a region drag from `start` to `end`. */
export function pickParams(session: string, start: Pt, end: Pt, region: boolean): { session: string; x: number; y: number; w?: number; h?: number } {
  const w = Math.abs(end.x - start.x);
  const h = Math.abs(end.y - start.y);
  if (!region || (w < DRAG_MIN_PX && h < DRAG_MIN_PX)) return { session, x: Math.round(end.x), y: Math.round(end.y) };
  return { session, x: Math.round(Math.min(start.x, end.x)), y: Math.round(Math.min(start.y, end.y)), w: Math.round(w), h: Math.round(h) };
}

/** Has the pointer moved far enough from where it went down to be a drag? */
export function dragged(start: Pt, now: Pt, px = DRAG_MIN_PX): boolean {
  return Math.abs(now.x - start.x) >= px || Math.abs(now.y - start.y) >= px;
}

/** The drag rectangle in percent of the page's viewport, whatever size the picture is drawn at. */
export function regionStyle(d: { a: Pt; b: Pt }, vp: Size): { left: string; top: string; width: string; height: string } {
  const w = vp.width > 0 ? vp.width : 1;
  const h = vp.height > 0 ? vp.height : 1;
  const pct = (n: number, of: number) => `${Math.min(100, Math.max(0, (n / of) * 100))}%`;
  return { left: pct(Math.min(d.a.x, d.b.x), w), top: pct(Math.min(d.a.y, d.b.y), h), width: pct(Math.abs(d.b.x - d.a.x), w), height: pct(Math.abs(d.b.y - d.a.y), h) };
}

/** A short title for a note in the list: its accessible name, else its text, else its selector. */
export function noteTitle(n: AnnotationNote): string {
  if (n.kind === 'page') return t.annotate.kind.page;
  const text = (n.name || n.text || '').trim().replace(/\s+/g, ' ');
  if (text) return text.length > 60 ? `${text.slice(0, 57)}…` : text;
  return n.selector || t.annotate.kind[n.kind];
}

/** Local screenshots by note number, without the notes that are gone from the draft. */
export function keepThumbs(thumbs: ReadonlyMap<number, string>, draft: AnnotationDraft | null): Map<number, string> {
  const live = new Set((draft?.notes ?? []).map((n) => n.n));
  return new Map([...thumbs].filter(([n]) => live.has(n)));
}

/** The result of sending: what to tell the user. */
export function sendOutcome(state: AnnotationState | undefined): 'delivered' | 'sending' | 'unconfirmed' | 'queued' {
  if (state === 'delivered' || state === 'sending' || state === 'unconfirmed') return state;
  return 'queued';
}

/** Why a queued draft waits, for a person: the host's `detail` without its "waiting: " prefix, first letter capital. */
export function waitReason(detail: string | null | undefined): string | null {
  const text = (detail ?? '').trim().replace(/^waiting:\s*/i, '').trim();
  if (!text) return null;
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** Drafts the notes panel can act on: the host keeps sent drafts around, but they are read-only history. */
export const isEditable = (d: AnnotationDraft | null): d is AnnotationDraft =>
  !!d && d.state !== 'delivered' && d.state !== 'sending' && d.state !== 'held';

export interface Annotations {
  draft: AnnotationDraft | null;
  busy: boolean;
  /** Notes in the draft (page note not counted). */
  count: number;
  refresh(): Promise<void>;
  /** Add a pick captured on this device (desktop). Resolves with the new note. */
  add(p: AnnotationPick & { url: string; title?: string; viewport?: { w: number; h: number; dpr?: number }; image_b64?: string }): Promise<AnnotationNote | null>;
  /** Ask the host to pick at a point or region of its own browser (phone). */
  pick(p: { x: number; y: number; w?: number; h?: number }): Promise<AnnotationNote | null>;
  edit(n: number, note: string): Promise<void>;
  setPageNote(note: string): Promise<void>;
  remove(n: number): Promise<void>;
  send(): Promise<void>;
  /** Owner only: deliver a queued draft now, skipping the idle, typing and input-box checks. */
  sendNow(): Promise<void>;
  copy(): Promise<void>;
  clear(): Promise<void>;
}

/**
 * The draft of notes for a preview (`preview`) or an agent browser session (`session`). Reads it on
 * mount and every few seconds while `active`, because the host may add notes from other surfaces.
 */
export function useAnnotations(hostId: string, kind: 'preview' | 'session', id: string | null, active: boolean): Annotations {
  const app = useApp();
  const connOf = useConnOf();
  const [draft, setDraft] = useState<AnnotationDraft | null>(null);
  const [busy, setBusy] = useState(false);
  const draftRef = useRef<AnnotationDraft | null>(null);
  draftRef.current = draft;
  const live = useRef(true);
  useEffect(() => {
    live.current = true;
    return () => {
      live.current = false;
    };
  }, []);

  const target = id ? (kind === 'preview' ? { preview: id } : { session: id }) : null;
  const key = `${hostId}:${kind}:${id ?? ''}`;

  const refresh = useCallback(async () => {
    const conn = connOf(hostId);
    if (!conn || !id) return;
    try {
      const r = await conn.request('browser.annotations', kind === 'preview' ? { preview: id } : { session: id });
      if (live.current) setDraft(r.draft);
    } catch {
      // keep what we have; the next poll tries again
    }
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [app, key]);

  useEffect(() => {
    setDraft(null);
    if (!active) return;
    void refresh();
    const timer = setInterval(() => void refresh(), POLL_MS);
    return () => clearInterval(timer);
  }, [active, refresh]);

  const run = useCallback(
    async <T,>(fn: () => Promise<T>): Promise<T | null> => {
      setBusy(true);
      try {
        return await fn();
      } catch (e) {
        app.toast(errorMessage(e), 'error');
        return null;
      } finally {
        if (live.current) setBusy(false);
      }
    },
    [app],
  );
  const conn = () => connOf(hostId);

  const add: Annotations['add'] = (p) =>
    run(async () => {
      const c = conn();
      if (!c || !target) return null;
      const r = await c.request('browser.annotation_add', { ...p, target });
      if (live.current) setDraft(r.draft);
      return r.note;
    });
  const pick: Annotations['pick'] = (p) =>
    run(async () => {
      const c = conn();
      if (!c || !id) return null;
      const r = await c.request('browser.annotation_pick', { session: id, ...p });
      if (live.current) setDraft(r.draft);
      return r.note;
    });
  const edit: Annotations['edit'] = async (n, note) => {
    const d = draftRef.current;
    if (!d) return;
    await run(async () => {
      const r = await conn()?.request('browser.annotation_update', { draft: d.id, n, note });
      if (r && live.current) setDraft(r.draft);
    });
  };
  const setPageNote: Annotations['setPageNote'] = async (note) => {
    const d = draftRef.current;
    if (!d) return;
    await run(async () => {
      const r = await conn()?.request('browser.annotation_update', { draft: d.id, page_note: note });
      if (r && live.current) setDraft(r.draft);
    });
  };
  const remove: Annotations['remove'] = async (n) => {
    const d = draftRef.current;
    if (!d) return;
    await run(async () => {
      const r = await conn()?.request('browser.annotation_remove', { draft: d.id, n });
      if (r && live.current) setDraft(r.draft);
    });
  };
  const send: Annotations['send'] = async () => {
    const d = draftRef.current;
    if (!d) return;
    await run(async () => {
      const r = await conn()?.request('browser.annotations_send', { draft: d.id });
      if (r) app.toast(t.annotate.sent[sendOutcome(r.state)], 'info');
      await refresh();
    });
  };
  const sendNow: Annotations['sendNow'] = async () => {
    const d = draftRef.current;
    if (!d) return;
    await run(async () => {
      const r = await conn()?.request('browser.annotations_send', { draft: d.id, force: true });
      if (r) app.toast(t.annotate.sent[sendOutcome(r.state)], 'info');
      await refresh();
    });
  };
  const copy: Annotations['copy'] = async () => {
    const d = draftRef.current;
    if (!d) return;
    await run(async () => {
      const r = await conn()?.request('browser.annotations_send', { draft: d.id, copy_only: true });
      if (r) {
        await app.platform.clipboard.writeText(r.text);
        app.toast(t.annotate.copied, 'info');
      }
    });
  };
  const clear: Annotations['clear'] = async () => {
    const d = draftRef.current;
    if (!d) return;
    await run(async () => {
      await conn()?.request('browser.annotations_clear', { draft: d.id });
      if (live.current) setDraft(null);
    });
  };

  return { draft, busy, count: draft?.notes.length ?? 0, refresh, add, pick, edit, setPageNote, remove, send, sendNow, copy, clear };
}
