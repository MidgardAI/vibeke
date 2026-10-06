// Native notifications from the main process (spec 16 §7.8, §16.2): one per host, merged, at the
// device's privacy level, suppressed while the app window is focused, DND respected. Clicking
// opens the card; on macOS the actions are "Approve…" (opens the quick popover on that card with
// a confirm) and "Open".
//
// Fails closed: nothing is shown for a host until its preferences (privacy level, toggles, DND)
// are known. Concurrent callers share one `prefs.get` per host; a failed refetch keeps the last
// known preferences. After awaiting them, what is shown is re-read from the tracker, so an
// interaction answered meanwhile is never announced.

import type { NotificationConstructorOptions } from 'electron';
import { RpcError, type HostState } from '@vibeke/core';
import { AlertTracker, DONE_DEBOUNCE_MS, alertAllowed, alertPrefsFrom, payloadFor, type DoneCandidate, type HostAlertChange, type HostAlertPrefs } from './alerts';
import type { Engine } from './engine';

export interface NotifierHooks {
  /** Suppress while the user is looking at the app (main window or popover focused). */
  appFocused(): boolean;
  enabled(): boolean;
  openMain(hash: string): void;
  approve(hash: string): void;
  icon?: string;
  /** Test seam: the clock (ms). */
  now?(): number;
}

/** Minimal surface of the engine the notifier uses (an `Engine`, or a fake in tests). */
export type NotifierEngine = Pick<Engine, 'request'> & { manager: { getSnapshot(): readonly HostState[]; subscribe(cb: () => void): () => void } | null };

/** Minimal surface of an OS notification (Electron's `Notification`, or a fake in tests). */
export interface NotificationLike {
  show(): void;
  close(): void;
  on(ev: 'click' | 'close', cb: () => void): unknown;
  on(ev: 'action', cb: (e: unknown, index: number) => void): unknown;
}

const PREFS_REFRESH_MS = 5 * 60_000;
/** Gateways without `prefs.get` (older builds): the most private behaviour. */
const LEGACY_PREFS: HostAlertPrefs = { privacy: 'minimal', notify_input: true, notify_done: false, dnd_until: 0 };
const METHOD_NOT_FOUND = -32601;

interface Shown {
  n: NotificationLike;
  hostId: string;
}

export class Notifier {
  private tracker = new AlertTracker();
  private shown = new Map<string, Shown>();
  private prefs = new Map<string, { p: HostAlertPrefs; at: number }>();
  /** One in-flight `prefs.get` per host, tagged with the invalidation generation it serves. */
  private pending = new Map<string, { gen: number; p: Promise<HostAlertPrefs | null> }>();
  private prefsGen = new Map<string, number>();
  /** Per-host sequence of merged-notification updates: only the newest renders after an await. */
  private seq = new Map<string, number>();
  /** Hosts with something new that has not been announced yet. */
  private wantsAlert = new Set<string>();
  private doneTimers = new Map<string, ReturnType<typeof setTimeout>>();
  private off: (() => void) | null = null;
  private stopped = false;

  constructor(
    private readonly engine: NotifierEngine,
    private readonly hooks: NotifierHooks,
    /** Electron's `new Notification(o)` (null when unsupported); Electron-free for tests. */
    private readonly create: (o: NotificationConstructorOptions) => NotificationLike | null,
  ) {}

  private now(): number {
    return this.hooks.now?.() ?? Date.now();
  }

  start(): void {
    const m = this.engine.manager;
    if (!m) return;
    this.stopped = false;
    const tick = () => this.onStates(m.getSnapshot());
    this.off = m.subscribe(tick);
    tick();
  }

  stop(): void {
    this.stopped = true;
    this.off?.();
    this.off = null;
    for (const t of this.doneTimers.values()) clearTimeout(t);
    this.doneTimers.clear();
    for (const s of this.shown.values()) s.n.close();
    this.shown.clear();
  }

  /** The renderer changed prefs on a host (prefs.set): refetch; the old ones stay until then. */
  invalidatePrefs(hostId: string): void {
    this.prefsGen.set(hostId, (this.prefsGen.get(hostId) ?? 0) + 1);
    const cur = this.prefs.get(hostId);
    if (cur) cur.at = 0;
    void this.prefsFor(hostId);
  }

  /** Known preferences for a host, or null while unknown (then nothing is shown). */
  prefsFor(hostId: string): Promise<HostAlertPrefs | null> {
    const cur = this.prefs.get(hostId);
    if (cur && this.now() - cur.at < PREFS_REFRESH_MS) return Promise.resolve(cur.p);
    const gen = this.prefsGen.get(hostId) ?? 0;
    const inflight = this.pending.get(hostId);
    if (inflight && inflight.gen === gen) return inflight.p;
    const entry: { gen: number; p: Promise<HostAlertPrefs | null> } = { gen, p: Promise.resolve(null) };
    entry.p = (async (): Promise<HostAlertPrefs | null> => {
      try {
        const r = await this.engine.request(hostId, 'prefs.get', {}, { timeoutMs: 10_000 });
        const v = alertPrefsFrom(r);
        if ((this.prefsGen.get(hostId) ?? 0) === gen) this.prefs.set(hostId, { p: v, at: this.now() });
        return this.prefs.get(hostId)?.p ?? v;
      } catch (e) {
        if (e instanceof RpcError && e.code === METHOD_NOT_FOUND) {
          this.prefs.set(hostId, { p: LEGACY_PREFS, at: this.now() });
          return LEGACY_PREFS;
        }
        // Offline / timeout: keep what we had (possibly restrictive); unknown stays unknown.
        return this.prefs.get(hostId)?.p ?? null;
      } finally {
        if (this.pending.get(hostId) === entry) this.pending.delete(hostId);
      }
    })();
    this.pending.set(hostId, entry);
    return entry.p;
  }

  private onStates(states: readonly HostState[]): void {
    for (const s of states) {
      const id = s.record.host_id;
      if (s.status === 'online' && !this.prefs.has(id) && !this.pending.has(id)) void this.prefsFor(id);
    }
    const { changes, done } = this.tracker.update(states);
    for (const c of changes) void this.apply(c);
    for (const d of done) this.scheduleDone(d);
    // Forgotten hosts: drop their notifications (merged and finished alike).
    const live = new Set(states.map((s) => s.record.host_id));
    for (const [tag, s] of [...this.shown]) if (!live.has(s.hostId)) this.close(tag);
  }

  private close(tag: string): void {
    this.shown.get(tag)?.n.close();
    this.shown.delete(tag);
  }

  private async apply(c: HostAlertChange): Promise<void> {
    const id = c.hostId;
    const seq = (this.seq.get(id) ?? 0) + 1;
    this.seq.set(id, seq);
    if (c.items.length === 0) {
      this.wantsAlert.delete(id);
      return this.close(id);
    }
    if (c.added) this.wantsAlert.add(id);
    const p = await this.prefsFor(id);
    if (this.stopped || this.seq.get(id) !== seq) return; // a newer update renders instead
    // Re-read: something may have been answered while the prefs were loading.
    const cur = this.tracker.current(id, c.hostName);
    const alert = this.wantsAlert.delete(id);
    if (cur.items.length === 0) return this.close(id);
    if (!p) return; // preferences unknown: fail closed
    const payload = payloadFor(cur, p);
    if (!payload) return;
    const approve = cur.approvable ? `#/i/${id}/${cur.approvable}?do=allow` : null;
    if (!alert) {
      // Only resolutions: refresh the shown notification's merged content, quietly.
      if (this.shown.has(id)) this.show(id, id, payload.title, payload.body, payload.url, approve, true);
      return;
    }
    if (!this.hooks.enabled() || this.hooks.appFocused()) return;
    const now = this.now();
    if (p.dnd_until * 1000 > now) return;
    if (cur.items.some((i) => i.urgent) && !alertAllowed(p, now, 'input')) return;
    this.show(id, id, payload.title, payload.body, payload.url, approve);
  }

  private scheduleDone(d: DoneCandidate): void {
    const key = `${d.hostId}/${d.runId}`;
    const prev = this.doneTimers.get(key);
    if (prev) clearTimeout(prev);
    this.doneTimers.set(
      key,
      setTimeout(async () => {
        this.doneTimers.delete(key);
        const p = await this.prefsFor(d.hostId);
        if (!p || this.stopped) return;
        // Confirm against the state *after* the await.
        const states = this.engine.manager?.getSnapshot() ?? [];
        const item = this.tracker.confirmDone(states, d);
        if (!item) return;
        if (!this.hooks.enabled() || this.hooks.appFocused() || !alertAllowed(p, this.now(), 'done')) return;
        const s = states.find((x) => x.record.host_id === d.hostId);
        const payload = payloadFor({ items: [item], hostName: s?.info?.host_name ?? s?.record.name ?? '' }, p);
        if (payload) this.show(`${d.hostId}:done`, d.hostId, payload.title, payload.body, payload.url, null, true);
      }, DONE_DEBOUNCE_MS),
    );
  }

  private show(tag: string, hostId: string, title: string, body: string, url: string, approveHash: string | null, quiet = false): void {
    const opts: NotificationConstructorOptions = {
      title,
      body,
      silent: quiet,
      urgency: quiet ? 'normal' : 'critical',
      timeoutType: quiet ? 'default' : 'never',
    };
    if (this.hooks.icon) opts.icon = this.hooks.icon;
    // macOS actions (shown for signed builds in alert style): Approve… only targets one card.
    if (process.platform === 'darwin' && approveHash) opts.actions = [{ type: 'button', text: 'Approve…' }, { type: 'button', text: 'Open' }];
    const n = this.create(opts);
    if (!n) return;
    this.close(tag);
    n.on('click', () => this.hooks.openMain(url));
    n.on('action', (_e, index) => {
      if (index === 0 && approveHash) this.hooks.approve(approveHash);
      else this.hooks.openMain(url);
    });
    n.on('close', () => {
      if (this.shown.get(tag)?.n === n) this.shown.delete(tag);
    });
    // Keep a reference: a garbage-collected Notification loses its click handlers.
    this.shown.set(tag, { n, hostId });
    n.show();
  }
}
