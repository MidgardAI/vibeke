// Keeps the sidebar order still while the user touches it (lib/stable-order.ts has the rules).
// Fresh data still reaches the rows in place; moves wait until the list has been quiet.

import { useEffect, useLayoutEffect, useRef, useState, type MouseEvent as ReactMouseEvent, type RefObject } from 'react';
import { FREEZE_QUIET_MS, MOVED_TAP_GUARD_MS, freezeRows, freezeSections, isFrozen, unfreezeDelay, type MemberRow, type Section } from '../lib/stable-order';
import type { WorkspaceList, WorkspaceRow, WorkspaceSection } from '../lib/workspaces';

export interface StableListHandlers {
  onPointerDownCapture(): void;
  onPointerUpCapture(): void;
  onPointerCancelCapture(): void;
  onScroll(): void;
  onWheel(): void;
}

export function useStableList(list: WorkspaceList): { list: WorkspaceList; handlers: StableListHandlers } {
  const [, bump] = useState(0);
  const touch = useRef({ down: false, last: 0 });
  const shown = useRef<Section<WorkspaceRow>[] | null>(null);
  const pending = useRef(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);

  const input = () => ({ pointerDown: touch.current.down, lastActivityMs: touch.current.last, nowMs: Date.now(), quietMs: FREEZE_QUIET_MS });
  const frozen = isFrozen(input());
  const next: Section<WorkspaceRow>[] = [{ id: 'pinned', rows: list.pinned }, ...list.groups];
  const sections = frozen ? freezeSections(shown.current, next) : next;
  shown.current = sections;
  pending.current = frozen;

  const schedule = () => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
    const d = unfreezeDelay(input());
    if (d !== null) {
      timer.current = setTimeout(() => {
        timer.current = null;
        if (pending.current) bump((n) => n + 1);
      }, d);
    }
  };
  // Every render (data changed while frozen) re-arms the timer; unmount clears it.
  useEffect(schedule);
  useEffect(
    () => () => {
      if (timer.current) clearTimeout(timer.current);
    },
    [],
  );

  const mark = (down: boolean | null) => {
    if (down !== null) touch.current.down = down;
    touch.current.last = Date.now();
    schedule();
  };
  const handlers: StableListHandlers = {
    onPointerDownCapture: () => mark(true),
    onPointerUpCapture: () => mark(false),
    onPointerCancelCapture: () => mark(false),
    onScroll: () => mark(null),
    onWheel: () => mark(null),
  };

  return {
    list: {
      ...list,
      pinned: sections.find((s) => s.id === 'pinned')?.rows ?? [],
      groups: sections.filter((s) => s.id !== 'pinned') as WorkspaceSection[],
    },
    handlers,
  };
}

export interface FrozenRowsView<R extends MemberRow> {
  rows: R[];
  /** Arrivals held back while the list is frozen. */
  held: number;
  /** Show held rows now (the "N new" pill). */
  release(): void;
  /** Spread on the list element. */
  listProps: {
    ref: RefObject<HTMLDivElement | null>;
    onPointerDownCapture(): void;
    onPointerUpCapture(): void;
    onPointerCancelCapture(): void;
    onClickCapture(e: ReactMouseEvent): void;
  };
}

/**
 * The inbox version of `useStableList`: rows keep their order while a pointer is down and for
 * `quietMs` after the last touch or scroll; arrivals wait behind an "N new" pill. A tap on a row
 * that moved within the last `MOVED_TAP_GUARD_MS` is ignored, so a card that slid under the
 * finger is never the one that gets the tap. Each rendered row needs `data-row-key={row.key}`.
 */
export function useFrozenRows<R extends MemberRow>(next: readonly R[], quietMs = 1000): FrozenRowsView<R> {
  const [, bump] = useState(0);
  const touch = useRef({ down: false, last: 0 });
  const shown = useRef<MemberRow[] | null>(null);
  const pending = useRef(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const ref = useRef<HTMLDivElement | null>(null);
  const tops = useRef(new Map<string, number>());
  const movedAt = useRef(new Map<string, number>());

  const input = () => ({ pointerDown: touch.current.down, lastActivityMs: touch.current.last, nowMs: Date.now(), quietMs });
  const frozen = isFrozen(input());
  const view = frozen ? freezeRows(shown.current, next) : { rows: [...next], held: 0 };
  shown.current = view.rows.map((r) => ({ key: r.key, members: r.members }));
  pending.current = frozen;

  const schedule = () => {
    if (timer.current) clearTimeout(timer.current);
    timer.current = null;
    const d = unfreezeDelay(input());
    if (d !== null) {
      timer.current = setTimeout(() => {
        timer.current = null;
        if (pending.current) bump((n) => n + 1);
      }, d);
    }
  };
  useEffect(schedule);
  useEffect(() => {
    // The scroller is an ancestor, and scroll events do not bubble: listen in the capture phase.
    const onScroll = (e: Event) => {
      const el = ref.current;
      if (!el || !(e.target instanceof Node) || !e.target.contains(el)) return;
      touch.current.last = Date.now();
      schedule();
    };
    document.addEventListener('scroll', onScroll, { capture: true, passive: true });
    return () => {
      document.removeEventListener('scroll', onScroll, { capture: true });
      if (timer.current) clearTimeout(timer.current);
    };
  }, []);

  // Note which rows moved on screen, for the tap guard.
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const now = Date.now();
    const seen = new Set<string>();
    const first = tops.current.size === 0;
    for (const row of Array.from(el.querySelectorAll<HTMLElement>('[data-row-key]'))) {
      const key = row.dataset.rowKey!;
      seen.add(key);
      const top = row.offsetTop;
      const before = tops.current.get(key);
      // The first layout is what the user sees arrive; later new or shifted rows moved.
      if (!first && before !== top) movedAt.current.set(key, now);
      tops.current.set(key, top);
    }
    for (const key of [...tops.current.keys()]) if (!seen.has(key)) (tops.current.delete(key), movedAt.current.delete(key));
  });

  const mark = (down: boolean | null) => {
    if (down !== null) touch.current.down = down;
    touch.current.last = Date.now();
    schedule();
  };
  return {
    rows: view.rows,
    held: view.held,
    release: () => {
      touch.current = { down: false, last: 0 };
      bump((n) => n + 1);
    },
    listProps: {
      ref,
      onPointerDownCapture: () => mark(true),
      onPointerUpCapture: () => mark(false),
      onPointerCancelCapture: () => mark(false),
      onClickCapture: (e) => {
        const row = (e.target as HTMLElement).closest?.('[data-row-key]') as HTMLElement | null;
        const at = row ? movedAt.current.get(row.dataset.rowKey!) : undefined;
        if (at !== undefined && Date.now() - at < MOVED_TAP_GUARD_MS) {
          e.preventDefault();
          e.stopPropagation();
        }
      },
    },
  };
}
