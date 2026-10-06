// Keyboard model of a tab list (WAI-ARIA tabs, manual activation): one tab is in the Tab order
// (roving tabindex), ←/→ move focus between tabs (wrapping), Home/End jump to the ends, Enter or
// Space selects the focused tab. Pure so it is unit tested; the strip applies it to the DOM.

/** The tab that takes part in the Tab order: the focused one, else the selected one, else the first. */
export function rovingTab(ids: readonly string[], selected: string | null, focused: string | null): string | null {
  if (focused && ids.includes(focused)) return focused;
  if (selected && ids.includes(selected)) return selected;
  return ids[0] ?? null;
}

/** Where a key moves focus from `from` (null: the key is not a tab-list key). */
export function tabKeyTarget(ids: readonly string[], from: string | null, key: string): string | null {
  if (!ids.length) return null;
  const i = from ? ids.indexOf(from) : -1;
  switch (key) {
    case 'ArrowRight':
      return ids[i < 0 ? 0 : (i + 1) % ids.length]!;
    case 'ArrowLeft':
      return ids[i < 0 ? ids.length - 1 : (i - 1 + ids.length) % ids.length]!;
    case 'Home':
      return ids[0]!;
    case 'End':
      return ids[ids.length - 1]!;
    default:
      return null;
  }
}

/** DOM ids tying each tab to the panel it controls. */
export const tabDomId = (id: string): string => `ws-tab-${id.replace(/[^A-Za-z0-9_-]/g, '_')}`;
export const TAB_PANEL_ID = 'ws-tabpanel';
