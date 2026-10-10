// What the menu-bar icon shows (spec 16 §16.2): how many agents need you across every connected
// host, and how many are working. An agent needs you when it has an open interaction, stopped
// (error / rate limited), or finished since the user last opened the popover. Pure and
// Electron-free; tray.ts draws it.

import type { HostState } from '@vibeke/core';

export interface TraySummary {
  /** Open interactions plus agents that stopped or finished unseen. */
  needs: number;
  working: number;
}

export class TrayTracker {
  /** done_rev per host and run the user has seen (popover opened, or first sight). */
  private seen = new Map<string, Map<string, number>>();

  summary(states: readonly HostState[]): TraySummary {
    let needs = 0;
    let working = 0;
    for (const id of [...this.seen.keys()]) if (!states.some((s) => s.record.host_id === id)) this.seen.delete(id);
    for (const s of states) {
      const d = s.dashboard;
      if (!d) continue;
      const prev = this.seen.get(s.record.host_id);
      // Rebuilt per dashboard, so ended runs are forgotten.
      const seen = new Map<string, number>();
      this.seen.set(s.record.host_id, seen);
      const open = d.interactions.filter((i) => i.status === 'open');
      needs += open.length;
      const asking = new Set(open.map((i) => i.run));
      for (const r of d.runs) {
        if (r.ended_at_ms !== null) continue;
        // A finish that happened before we first saw the run is old news.
        const rev = prev?.get(r.id) ?? r.done_rev;
        seen.set(r.id, rev);
        if (asking.has(r.id)) continue;
        switch (r.execution.value) {
          case 'working':
          case 'starting':
            working++;
            break;
          case 'error':
          case 'rate_limited':
            needs++;
            break;
          case 'idle':
            if (r.done_rev > rev) needs++;
            break;
        }
      }
    }
    return { needs, working };
  }

  /** The user looked (opened the popover): finished agents stop counting. */
  markSeen(states: readonly HostState[]): void {
    for (const s of states) {
      const seen = this.seen.get(s.record.host_id);
      if (seen) for (const r of s.dashboard?.runs ?? []) if (seen.has(r.id)) seen.set(r.id, r.done_rev);
    }
  }
}
