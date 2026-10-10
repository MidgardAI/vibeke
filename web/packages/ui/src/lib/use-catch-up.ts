// React side of the catch-up: one store per app (lib/catch-up-store.ts), a tracker that records
// when the window goes to the background, and a hook that gives the inbox its cards.

import { useCallback, useEffect, useMemo, useRef } from 'react';
import type { AppModel } from '../app/model';
import { useApp } from '../app/hooks';
import { useWorkspaceRows } from '../app/selection';
import type { CatchUpCard } from './catch-up';
import { CatchUpStore } from './catch-up-store';
import { useStore } from './store';

const stores = new WeakMap<AppModel, CatchUpStore>();

export function catchUpStore(app: AppModel): CatchUpStore {
  let s = stores.get(app);
  if (!s) {
    s = new CatchUpStore(app.platform.kv, () => app.platform.clock.now());
    s.load();
    stores.set(app, s);
  }
  return s;
}

/** Mounted once for the main window: notes when the app goes to the background and comes back. */
export function useCatchUpTracking(): void {
  const app = useApp();
  const rows = useWorkspaceRows();
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  useEffect(() => {
    const store = catchUpStore(app);
    const lc = app.platform.lifecycle;
    const offs = [lc.onHidden(() => store.hidden(rowsRef.current, store.cards(rowsRef.current).length)), lc.onVisible(() => store.visible())];
    return () => offs.forEach((f) => f());
  }, [app]);
}

export function useCatchUp(): { cards: CatchUpCard[]; dismiss(card: CatchUpCard): void; dismissAll(): void } {
  const app = useApp();
  const store = catchUpStore(app);
  const state = useStore(store.state);
  const rows = useWorkspaceRows();
  const cards = useMemo(() => store.cards(rows), [store, state, rows]);
  const cardsRef = useRef(cards);
  cardsRef.current = cards;
  const rowsRef = useRef(rows);
  rowsRef.current = rows;
  const dismiss = useCallback((c: CatchUpCard) => store.dismiss(rowsRef.current, [c]), [store]);
  const dismissAll = useCallback(() => store.dismiss(rowsRef.current, cardsRef.current), [store]);
  return { cards, dismiss, dismissAll };
}
