// j/k selection over the current screen's list (inbox cards, else the sidebar's workspaces). Lists mark themselves
// with `data-nav-list`, items with `data-nav-item="<stable key>"`, and the buttons a key may
// press with `data-act="allow|deny|allow_always|open"`. Keys press the same buttons a pointer
// would, so confirmations (high/unknown risk, allow always) stay in the path.
//
// The selection follows the user's attention: focusing or clicking inside an item selects it, so
// `a` always acts on the card the user is on. When the selected item disappears (answered,
// settled) its successor is only highlighted; the next action key confirms that selection
// instead of acting, so holding or repeating a key can never answer the following card.

export type NavAct = 'allow' | 'deny' | 'allow_always' | 'open';

class ListNav {
  private key: string | null = null;
  private index = 0;
  /** The user chose the current selection (j/k, focus, pointer); false after an auto-advance. */
  private intended = false;
  private observer: MutationObserver | null = null;
  private observed: Element | null = null;
  private attached = 0;
  private readonly onAttention = (e: Event) => {
    const t = e.target instanceof Element ? e.target : null;
    const item = t?.closest<HTMLElement>('[data-nav-item]');
    const root = item?.closest('[data-nav-list]');
    if (!item || !root || root !== this.root()) return;
    if (item.dataset.navItem === this.key && this.intended) return;
    this.select(item, this.items(root), { focus: false, scroll: false });
  };

  /** Follow focus and pointer interaction (once per window; returns a detach function). */
  attach(): () => void {
    if (typeof document === 'undefined') return () => {};
    if (this.attached++ === 0) {
      document.addEventListener('focusin', this.onAttention, true);
      document.addEventListener('pointerdown', this.onAttention, true);
    }
    return () => {
      if (--this.attached === 0) {
        document.removeEventListener('focusin', this.onAttention, true);
        document.removeEventListener('pointerdown', this.onAttention, true);
      }
    };
  }

  private root(): Element | null {
    if (typeof document === 'undefined') return null;
    // The screen's own visible list (inbox cards…) wins; otherwise the sidebar's workspace rows
    // (`data-nav-list="sidebar"`). Dialogs never contain one.
    const lists = [...document.querySelectorAll('[data-nav-list]')].filter((el) => !el.closest('[inert]') && el.getClientRects().length > 0);
    return lists.find((el) => el.getAttribute('data-nav-list') !== 'sidebar') ?? lists[0] ?? null;
  }

  private items(root: Element): HTMLElement[] {
    return [...root.querySelectorAll<HTMLElement>('[data-nav-item]')].filter((el) => el.getClientRects().length > 0);
  }

  private current(root: Element): HTMLElement | null {
    if (this.key === null) return null;
    return this.items(root).find((el) => el.dataset.navItem === this.key) ?? null;
  }

  private select(el: HTMLElement | null, items: HTMLElement[], o: { focus?: boolean; scroll?: boolean } = {}): void {
    for (const x of items) if (x !== el && x.hasAttribute('data-selected')) x.removeAttribute('data-selected');
    if (!el) {
      this.key = null;
      this.intended = false;
      return;
    }
    this.key = el.dataset.navItem ?? null;
    this.index = Math.max(0, items.indexOf(el));
    this.intended = true;
    el.setAttribute('data-selected', '');
    if (o.scroll !== false) el.scrollIntoView?.({ block: 'nearest' });
    if (o.focus !== false && document.activeElement !== el && !el.contains(document.activeElement)) el.focus({ preventScroll: true });
    this.watch(el.closest('[data-nav-list]'));
  }

  /** When the selected item leaves (answered elsewhere, settled), highlight its successor. */
  private watch(root: Element | null): void {
    if (root === this.observed || typeof MutationObserver === 'undefined') return;
    this.observer?.disconnect();
    this.observed = root;
    if (!root) return;
    this.observer = new MutationObserver(() => {
      if (this.key === null || !root.isConnected) return;
      const items = this.items(root);
      if (items.some((el) => el.dataset.navItem === this.key)) return;
      const next = items[Math.min(this.index, items.length - 1)];
      if (next) {
        // Highlight only; do not steal focus from whatever the user is doing. Not intended yet:
        // the next action key confirms this selection rather than acting on it.
        for (const x of items) x.removeAttribute('data-selected');
        this.key = next.dataset.navItem ?? null;
        this.intended = false;
        next.setAttribute('data-selected', '');
      } else {
        this.key = null;
        this.intended = false;
      }
    });
    this.observer.observe(root, { childList: true, subtree: true });
  }

  /** Move the selection by `delta` (+1 = j, -1 = k). Returns false when there is no list. */
  move(delta: number): boolean {
    const root = this.root();
    if (!root) return false;
    const items = this.items(root);
    if (!items.length) return false;
    const cur = this.current(root);
    const at = cur ? items.indexOf(cur) : -1;
    // Nothing selected yet (or it left): start where the last selection was, else at the top.
    const next = at < 0 ? Math.min(this.index, items.length - 1) : Math.max(0, Math.min(items.length - 1, at + delta));
    this.select(items[next]!, items);
    return true;
  }

  /** Select a specific item by key (e.g. a notification's card). */
  selectKey(key: string): boolean {
    const root = this.root();
    if (!root) return false;
    const items = this.items(root);
    const el = items.find((x) => x.dataset.navItem === key);
    if (!el) return false;
    this.select(el, items);
    return true;
  }

  /**
   * Press the selected item's `act` button. With nothing selected (or the selection gone), or a
   * selection the app moved there by itself, the first key only selects: an action never lands
   * on a card the user has not chosen.
   */
  act(act: NavAct): 'done' | 'selected' | 'unavailable' | 'none' {
    const root = this.root();
    if (!root) return 'none';
    const cur = this.current(root);
    if (!cur) return this.move(1) ? 'selected' : 'none';
    if (!this.intended) {
      this.select(cur, this.items(root));
      return 'selected';
    }
    const btn = cur.querySelector<HTMLElement>(`[data-act="${act}"]`) ?? (act === 'open' && cur.tagName === 'BUTTON' ? cur : null);
    if (!btn || (btn as HTMLButtonElement).disabled) return 'unavailable';
    btn.click();
    return 'done';
  }

  /** Forget the selection (route changed). */
  clear(): void {
    this.key = null;
    this.index = 0;
    this.intended = false;
    this.observer?.disconnect();
    this.observer = null;
    this.observed = null;
  }
}

export const listNav = new ListNav();
