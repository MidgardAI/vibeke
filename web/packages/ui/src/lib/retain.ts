// Inbox cards outlive their `open` status briefly: an answer from this device stays to show its
// delivery state (delivering → delivered, or failed/unknown until looked at), and a card answered
// elsewhere leaves with "answered in terminal" (spec 16 §9.2).

import type { InboxItem, Interaction } from '@vibeke/core';
import { deliveryView, isSettled, type LocalAnswer } from './answer';

export type EntryMode = 'open' | 'answered' | 'leaving';

export interface RetainedEntry {
  key: string;
  item: InboxItem;
  mode: EntryMode;
}

interface Entry extends RetainedEntry {
  since: number;
  settledAt: number | null;
  index: number;
}

export const LEAVE_MS = 1600;
export const SETTLED_MS = 2500;
export const MAX_RETAIN_MS = 5 * 60_000;

export const itemKey = (it: InboxItem): string => `${it.host_id}/${it.interaction.id}`;

export class InboxRetainer {
  private entries = new Map<string, Entry>();

  update(
    open: readonly InboxItem[],
    lookup: (host: string, id: string) => Interaction | undefined,
    local: (key: string) => LocalAnswer | undefined,
    now: number,
  ): RetainedEntry[] {
    const openKeys = new Set<string>();
    open.forEach((item, index) => {
      const key = itemKey(item);
      openKeys.add(key);
      const prev = this.entries.get(key);
      this.entries.set(key, { key, item, mode: 'open', since: prev?.mode === 'open' ? prev.since : now, settledAt: null, index });
    });
    for (const [key, e] of [...this.entries]) {
      if (openKeys.has(key)) continue;
      const mine = local(key);
      const live = lookup(e.item.host_id, e.item.interaction.id) ?? e.item.interaction;
      if (mine) {
        const item = { ...e.item, interaction: live };
        const settled = isSettled(deliveryView(mine, live));
        const settledAt = settled ? (e.settledAt ?? now) : null;
        const since = e.mode === 'answered' ? e.since : now;
        if ((settledAt !== null && now - settledAt >= SETTLED_MS) || now - since > MAX_RETAIN_MS) {
          this.entries.delete(key);
          continue;
        }
        this.entries.set(key, { ...e, item, mode: 'answered', since, settledAt });
      } else if (e.mode === 'open') {
        this.entries.set(key, { ...e, item: { ...e.item, interaction: live }, mode: 'leaving', since: now });
      } else if (e.mode === 'leaving' && now - e.since >= LEAVE_MS) {
        this.entries.delete(key);
      } else if (e.mode === 'answered') {
        // Local state was cleared (e.g. stale refresh) and it is not open: drop.
        this.entries.delete(key);
      }
    }
    const out: RetainedEntry[] = open.map((item) => this.entries.get(itemKey(item))!);
    const retained = [...this.entries.values()].filter((e) => e.mode !== 'open').sort((a, b) => a.index - b.index);
    for (const e of retained) out.splice(Math.min(e.index, out.length), 0, e);
    return out.map(({ key, item, mode }) => ({ key, item, mode }));
  }

  /** Drop an answered card the user dismissed. */
  dismiss(key: string): void {
    this.entries.delete(key);
  }
}
