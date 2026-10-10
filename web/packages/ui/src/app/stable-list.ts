// Keeps the sidebar order still while the user touches it (lib/stable-order.ts has the rules).
// Fresh data still reaches the rows in place; moves wait until the list has been quiet.

import { useEffect, useRef, useState } from 'react';
import { FREEZE_QUIET_MS, freezeSections, isFrozen, unfreezeDelay, type Section } from '../lib/stable-order';
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
