// Native notifications from the main process (spec 16 §7.8, §16.2): one per host, merged, at the
// device's privacy level, suppressed while the app window is focused, DND respected. Clicking
// opens the card; on macOS the actions are "Approve…" (opens the quick popover on that card with
// a confirm) and "Open".
//
// Fails closed: nothing is shown for a host until its preferences (privacy level, toggles, DND)
// of the current generation are known; a response superseded by `invalidatePrefs` is never used.
// Concurrent callers share one `prefs.get` per host. An alert that arrives while preferences are
// unknown stays pending and is replayed once a (backed-off) retry loads them. After awaiting,
// what is shown is re-read from the tracker, so an interaction answered meanwhile is never
// announced; quiet resolution updates pass the same gates (enabled, focus, DND, toggles) as a
// new alert and withdraw the notification when gated.

import type { NotificationConstructorOptions } from 'electron';
import { RpcError, type AppEvent, type HostState } from '@vibeke/core';
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

/** What an approved call does, for the alert title (mirrors the UI's approvals verbs). */
const APPROVAL_VERBS: Record<string, string> = { 'handoff.send': 'send a handoff', 'handoff.cancel': 'cancel a handoff', 'gateway.call': 'redeem a peer invitation' };

const PREFS_REFRESH_MS = 5 * 60_000;
/** Gateways without `prefs.get` (older builds): the most private behaviour. */
const LEGACY_PREFS: HostAlertPrefs = { privacy: 'minimal', notify_input: true, notify_done: false, dnd_until: 0 };
const METHOD_NOT_FOUND = -32601;
const PREFS_RETRY_BASE_MS = 2_000;
const PREFS_RETRY_MAX_MS = 60_000;

interface Shown {
  n: NotificationLike;
  hostId: string;
}

export class Notifier {
  private tracker = new AlertTracker();
  private shown = new Map<string, Shown>();
  /** Last loaded preferences per host, with the invalidation generation they belong to. */
  private prefs = new Map<string, { p: HostAlertPrefs; at: number; gen: number }>();
  /** One in-flight `prefs.get` per host, tagged with the invalidation generation it serves. */
  private pending = new Map<string, { gen: number; p: Promise<HostAlertPrefs | null> }>();
  private prefsGen = new Map<string, number>();
  /** Per-host sequence of merged-notification updates: only the newest renders after an await. */
  private seq = new Map<string, number>();
  /** Hosts with something new that has not been announced yet. */
  private wantsAlert = new Set<string>();
  /** Backoff retries of `prefs.get` while an alert waits for preferences. */
  private retries = new Map<string, { attempt: number; timer: ReturnType<typeof setTimeout> | null }>();
  private doneTimers = new Map<string, ReturnType<typeof setTimeout>>();
  /** Pane approval requests awaiting a decision (notification tags). */
  private approvals = new Set<string>();
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
    for (const id of [...this.retries.keys()]) this.clearRetry(id);
    for (const s of this.shown.values()) s.n.close();
    this.shown.clear();
    this.approvals.clear();
  }

  /**
   * Host events: a pane's approval request raises its own alert (click opens the review screen,
   * no decision buttons); granted / denied / withdrawn withdraw it. Same gates as other input alerts.
   */
  onEvent(hostId: string, e: AppEvent): void {
    const request = e.subject.request;
    if (!request || this.stopped) return;
    const tag = `${hostId}:approval:${request}`;
    if (e.type === 'auth.approval_granted' || e.type === 'auth.approval_denied' || e.type === 'auth.approval_withdrawn') {
      this.approvals.delete(tag);
      this.close(tag);
      return;
    }
    if (e.type !== 'auth.approval_requested') return;
    this.approvals.add(tag);
    const method = typeof e.data.method === 'string' ? e.data.method : '';
    const summary = typeof e.data.summary === 'string' ? e.data.summary : '';
    void (async () => {
      const p = await this.prefsFor(hostId);
      if (!p || this.stopped || !this.approvals.has(tag)) return; // unknown prefs fail closed; or ended meanwhile
      if (!this.hooks.enabled() || this.hooks.appFocused() || !alertAllowed(p, this.now(), 'input')) return;
      const title = `A pane asks to ${APPROVAL_VERBS[method] ?? 'run a call'}`;
      const body = p.privacy === 'minimal' ? '' : summary;
      this.show(tag, hostId, title, body, `#/approve/${encodeURIComponent(hostId)}/${encodeURIComponent(request)}`, null);
    })();
  }

  /** The renderer changed prefs on a host (prefs.set): refetch; until then nothing is delivered. */
  invalidatePrefs(hostId: string): void {
    this.prefsGen.set(hostId, this.gen(hostId) + 1);
    void this.prefsFor(hostId);
  }

  private gen(hostId: string): number {
    return this.prefsGen.get(hostId) ?? 0;
  }

  /**
   * Preferences of the host's *current* generation, or null while unknown (then nothing is shown).
   * A response that was superseded by an invalidation meanwhile is never returned: its waiters
   * await the current generation instead. A failed fetch returns the cached preferences only when
   * they belong to the current generation (a periodic refresh failing); after an invalidation the
   * new preferences are unknown, so delivery is suppressed until a retry loads them.
   */
  prefsFor(hostId: string): Promise<HostAlertPrefs | null> {
    const gen = this.gen(hostId);
    const cur = this.prefs.get(hostId);
    if (cur && cur.gen === gen && this.now() - cur.at < PREFS_REFRESH_MS) return Promise.resolve(cur.p);
    const inflight = this.pending.get(hostId);
    if (inflight && inflight.gen === gen) return inflight.p;
    const entry: { gen: number; p: Promise<HostAlertPrefs | null> } = { gen, p: Promise.resolve(null) };
    entry.p = (async (): Promise<HostAlertPrefs | null> => {
      let got: HostAlertPrefs | null = null;
      try {
        got = alertPrefsFrom(await this.engine.request(hostId, 'prefs.get', {}, { timeoutMs: 10_000 }));
      } catch (e) {
        if (e instanceof RpcError && e.code === METHOD_NOT_FOUND) got = LEGACY_PREFS;
      } finally {
        if (this.pending.get(hostId) === entry) this.pending.delete(hostId);
      }
      if (this.stopped) return null;
      // Superseded while in flight: never deliver with it; follow the current generation.
      if (this.gen(hostId) !== gen) return this.prefsFor(hostId);
      if (got) {
        this.prefs.set(hostId, { p: got, at: this.now(), gen });
        this.clearRetry(hostId);
        // Something is still waiting to be announced (an earlier fetch failed): replay it.
        if (this.wantsAlert.has(hostId)) this.replay(hostId);
        return got;
      }
      // Offline / timeout: the current generation's last known preferences, else unknown.
      const known = this.prefs.get(hostId);
      return known && known.gen === gen ? known.p : null;
    })();
    this.pending.set(hostId, entry);
    return entry.p;
  }

  /** Preferences could not be loaded while an alert waits: try again with backoff. */
  private scheduleRetry(hostId: string): void {
    if (this.stopped || this.retries.get(hostId)?.timer) return;
    const attempt = this.retries.get(hostId)?.attempt ?? 0;
    const delay = Math.min(PREFS_RETRY_MAX_MS, PREFS_RETRY_BASE_MS * 2 ** attempt);
    const timer = setTimeout(async () => {
      this.retries.set(hostId, { attempt: attempt + 1, timer: null });
      if (this.stopped || !this.wantsAlert.has(hostId)) return this.clearRetry(hostId);
      const p = await this.prefsFor(hostId); // success replays via prefsFor
      if (!p && this.wantsAlert.has(hostId)) this.scheduleRetry(hostId);
    }, delay);
    (timer as { unref?: () => void }).unref?.();
    this.retries.set(hostId, { attempt, timer });
  }

  private clearRetry(hostId: string): void {
    const r = this.retries.get(hostId);
    if (r?.timer) clearTimeout(r.timer);
    this.retries.delete(hostId);
  }

  /** Re-evaluate a host's still-open items (after preferences finally loaded). */
  private replay(hostId: string): void {
    const s = this.engine.manager?.getSnapshot().find((x) => x.record.host_id === hostId);
    if (!s) return void this.wantsAlert.delete(hostId);
    void this.apply(this.tracker.current(hostId, s.info?.host_name ?? s.record.name));
  }

  /** May this host's merged notification be on screen right now (new alert or quiet update)? */
  private allowed(items: readonly { urgent: boolean }[], p: HostAlertPrefs): boolean {
    if (!this.hooks.enabled() || this.hooks.appFocused()) return false;
    const now = this.now();
    if (p.dnd_until * 1000 > now) return false;
    if (items.some((i) => i.urgent) && !alertAllowed(p, now, 'input')) return false;
    return true;
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
      this.clearRetry(id);
      return this.close(id);
    }
    if (c.added) this.wantsAlert.add(id);
    const p = await this.prefsFor(id);
    if (this.stopped || this.seq.get(id) !== seq) return; // a newer update renders instead
    // Re-read: something may have been answered while the prefs were loading.
    const cur = this.tracker.current(id, c.hostName);
    if (cur.items.length === 0) {
      this.wantsAlert.delete(id);
      return this.close(id);
    }
    if (!p) {
      // Preferences unknown: fail closed, but keep the alert pending and retry; a shown
      // notification (rendered under older preferences) is withdrawn rather than refreshed.
      this.close(id);
      if (this.wantsAlert.has(id)) this.scheduleRetry(id);
      return;
    }
    const alert = this.wantsAlert.delete(id);
    const payload = payloadFor(cur, p);
    if (!payload || !this.allowed(cur.items, p)) return this.close(id);
    const approve = cur.approvable ? `#/i/${id}/${cur.approvable}?do=allow` : null;
    if (!alert) {
      // Only resolutions: refresh the shown notification's merged content, quietly.
      if (this.shown.has(id)) this.show(id, id, payload.title, payload.body, payload.url, approve, true);
      return;
    }
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
