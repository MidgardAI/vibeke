// What deserves a native notification (spec 16 §7.8, §16.2), derived from live dashboards in the
// main process: newly opened interactions (approval / question / plan review), agents that
// stopped (error / rate limited) and agents that finished. Pure and Electron-free; notifier.ts
// turns the changes into OS notifications.

import { alertPayload, describeInteraction, describeRun, isPrivacyLevel, type AlertItem, type AlertPayload, type Execution, type HostState, type PrivacyLevel } from '@vibeke/core';

const ALERT_KINDS = new Set(['approval', 'question', 'plan_review']);
/** "Finished" waits this long and is dropped if the agent is working again (§7.8). */
export const DONE_DEBOUNCE_MS = 30_000;

export interface HostAlertPrefs {
  privacy: PrivacyLevel;
  notify_input: boolean;
  notify_done: boolean;
  /** Unix seconds; host-wide do-not-disturb. */
  dnd_until: number;
}

export const DEFAULT_ALERT_PREFS: HostAlertPrefs = { privacy: 'summary', notify_input: true, notify_done: false, dnd_until: 0 };

/** Parse a `prefs.get` result leniently. */
export function alertPrefsFrom(r: unknown): HostAlertPrefs {
  const o = (r ?? {}) as { device?: Record<string, unknown>; host?: Record<string, unknown> };
  const d = o.device ?? {};
  const h = o.host ?? {};
  return {
    privacy: isPrivacyLevel(d.privacy) ? d.privacy : DEFAULT_ALERT_PREFS.privacy,
    notify_input: typeof d.notify_input === 'boolean' ? d.notify_input : DEFAULT_ALERT_PREFS.notify_input,
    notify_done: typeof d.notify_done === 'boolean' ? d.notify_done : DEFAULT_ALERT_PREFS.notify_done,
    dnd_until: typeof h.dnd_until === 'number' && Number.isFinite(h.dnd_until) ? h.dnd_until : 0,
  };
}

export interface HostAlertChange {
  hostId: string;
  hostName: string;
  /** Everything currently worth showing for this host (merged into one notification). */
  items: AlertItem[];
  /** Something new arrived (show / re-alert); otherwise items only resolved. */
  added: boolean;
  /** The single item when exactly one is open: an interaction id that "Approve…" may target. */
  approvable: string | null;
}

export interface DoneCandidate {
  hostId: string;
  runId: string;
  doneRev: number;
}

interface HostTrack {
  baseline: boolean;
  open: Set<string>;
  items: Map<string, AlertItem>;
  exec: Map<string, { value: Execution; done: number }>;
}

export class AlertTracker {
  private hosts = new Map<string, HostTrack>();

  /** Feed the latest host states; returns per-host changes and "maybe finished" runs. */
  update(states: readonly HostState[]): { changes: HostAlertChange[]; done: DoneCandidate[] } {
    const changes: HostAlertChange[] = [];
    const done: DoneCandidate[] = [];
    const live = new Set<string>();
    for (const s of states) {
      const id = s.record.host_id;
      live.add(id);
      const d = s.dashboard;
      if (!d) continue;
      let tr = this.hosts.get(id);
      if (!tr) this.hosts.set(id, (tr = { baseline: false, open: new Set(), items: new Map(), exec: new Map() }));
      const hostName = s.info?.host_name ?? s.record.name;
      let added = false;
      let removed = false;

      // Interactions.
      const open = d.interactions.filter((i) => i.status === 'open' && ALERT_KINDS.has(i.kind));
      const now = new Set(open.map((i) => i.id));
      for (const i of open) {
        if (tr.open.has(i.id) || !tr.baseline) continue;
        tr.items.set(i.id, { key: i.id, title: describeInteraction(d, i), url: `#/i/${id}/${i.id}`, urgent: true });
        added = true;
      }
      for (const key of [...tr.items.keys()]) {
        if (!key.startsWith('run:') && !now.has(key)) {
          tr.items.delete(key);
          removed = true;
        }
      }
      tr.open = now;

      // Runs: stopped (error / rate limited) and finished.
      for (const r of d.runs) {
        const prev = tr.exec.get(r.id);
        const cur = { value: r.execution.value, done: r.done_rev };
        tr.exec.set(r.id, cur);
        if (!tr.baseline || !prev) continue;
        const k = `run:${r.id}`;
        if (cur.value !== prev.value && (cur.value === 'error' || cur.value === 'rate_limited')) {
          const what = cur.value === 'error' ? 'stopped with an error' : 'is rate limited';
          tr.items.set(k, { key: k, title: `${describeRun(d, r)} ${what}`, url: `#/r/${id}/${r.id}`, urgent: false });
          added = true;
        } else if ((cur.value === 'working' || cur.value === 'starting') && tr.items.delete(k)) {
          removed = true;
        }
        if (cur.value === 'idle' && cur.done > prev.done) done.push({ hostId: id, runId: r.id, doneRev: cur.done });
      }
      tr.baseline = true;

      if (added || removed) {
        const items = [...tr.items.values()];
        const only = items.length === 1 && !items[0]!.key.startsWith('run:') ? items[0]!.key : null;
        changes.push({ hostId: id, hostName, items, added, approvable: only });
      }
    }
    for (const id of [...this.hosts.keys()]) if (!live.has(id)) this.hosts.delete(id);
    return { changes, done };
  }

  /** What a host's merged notification should say right now (after an await, re-read this). */
  current(hostId: string, hostName: string): HostAlertChange {
    const items = [...(this.hosts.get(hostId)?.items.values() ?? [])];
    const only = items.length === 1 && !items[0]!.key.startsWith('run:') ? items[0]!.key : null;
    return { hostId, hostName, items, added: false, approvable: only };
  }

  /** After the debounce: is this run still finished with nothing open? → payload items. */
  confirmDone(states: readonly HostState[], c: DoneCandidate): AlertItem | null {
    const s = states.find((x) => x.record.host_id === c.hostId);
    const d = s?.dashboard;
    const r = d?.runs.find((x) => x.id === c.runId);
    if (!d || !r || r.execution.value !== 'idle' || r.done_rev < c.doneRev) return null;
    if (d.interactions.some((i) => i.run === r.id && i.status === 'open')) return null;
    return { key: `run:${r.id}`, title: `${describeRun(d, r)} finished`, url: `#/r/${c.hostId}/${r.id}`, urgent: false };
  }
}

/** Should anything be shown right now? (Focus suppression is the caller's.) */
export function alertAllowed(p: HostAlertPrefs, nowMs: number, kind: 'input' | 'done'): boolean {
  if (p.dnd_until * 1000 > nowMs) return false;
  return kind === 'input' ? p.notify_input : p.notify_done;
}

export function payloadFor(change: Pick<HostAlertChange, 'items' | 'hostName'>, p: HostAlertPrefs): AlertPayload | null {
  return alertPayload(change.items, p.privacy, change.hostName);
}
