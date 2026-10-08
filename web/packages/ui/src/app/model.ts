// The app's root model: device key, host manager, push sync, prefs and the answer flow. Created
// once per shell bootstrap and handed to React through context.

import {
  HostManager,
  fingerprint,
  x25519Public,
  PushSync,
  answerParams,
  batchAnswerParams,
  loadOrCreateDeviceKey,
  normalizeInteraction,
  pair as corePair,
  pairingKey,
  type Batch,
  type Decision,
  type HostConnectionApi,
  type HostManagerApi,
  type HostRecord,
  type Interaction,
  type PairingLink,
} from '@vibeke/core';
import { t } from '../i18n';
import { isPickerChanged } from '../lib/pickers';
import { AnswerStore, classifyError, errorMessage, staleInteraction } from '../lib/answer';
import { badgeCount, staleTags } from '../lib/notify';
import { PrefsStore } from '../lib/prefs';
import { ValueStore } from '../lib/store';
import { runKey } from '../lib/tree';
import type { CachedMirror, HapticKind, UiPlatform } from '../platform';
import type { TimerHandle } from '@vibeke/core';

export type Phase = 'loading' | 'ready' | 'error';

/** Answered interactions kept for their delivery state after they leave the dashboard. */
export const MAX_FINALS = 200;

export interface Toast {
  id: number;
  text: string;
  tone: 'info' | 'ok' | 'warn' | 'error';
}

export interface AnswerParams {
  decision?: Decision;
  choices?: Record<string, string[]>;
  text?: string;
  expected_signature?: string;
}

export class AppModel {
  readonly prefs: PrefsStore;
  readonly answers = new AnswerStore();
  readonly phase = new ValueStore<Phase>('loading');
  readonly locked = new ValueStore(false);
  readonly toasts = new ValueStore<Toast[]>([]);
  readonly mirrors = new Map<string, CachedMirror>();
  /**
   * Final state of interactions this device answered. Dashboards only carry open interactions, so
   * once an answered one leaves the snapshot its delivery state comes from `interaction.get`.
   */
  readonly finals = new Map<string, Interaction>();
  /** Is this window shown? Display-only timers (wait times, banners) pause while it is not. */
  readonly visible: ValueStore<boolean>;
  /** Delivery polls in flight (cancelled on stop: a closed window must not keep polling). */
  private deliveryTimers = new Set<TimerHandle>();
  private stopped = false;
  error: string | null = null;
  private _manager: HostManagerApi | null = null;
  private fingerprint: string | null = null;
  private _push: PushSync | null = null;
  private devicePrivate: Uint8Array | null = null;
  private toastSeq = 0;
  private offs: (() => void)[] = [];

  constructor(readonly platform: UiPlatform) {
    this.prefs = new PrefsStore(platform.kv);
    this.visible = new ValueStore(platform.lifecycle?.isVisible() ?? true);
    if (!this.prefs.get().deviceName) this.prefs.patch({ deviceName: platform.defaultDeviceName });
  }

  get manager(): HostManagerApi {
    if (!this._manager) throw new Error('app not started');
    return this._manager;
  }
  get push(): PushSync {
    if (!this._push) throw new Error('app not started');
    return this._push;
  }

  async start(): Promise<void> {
    this.stopped = false;
    const lc = this.platform.lifecycle;
    this.offs.push(lc.onVisible(() => this.visible.set(true)), lc.onHidden(() => this.visible.set(false)));
    this.visible.set(lc.isVisible());
    try {
      const p = this.platform;
      if (p.engine) {
        // Connections live outside this window (Electron main process); keys never enter it.
        const r = await p.engine.start();
        this._manager = r.manager;
        this.fingerprint = r.fingerprint;
      } else {
        this.devicePrivate = await loadOrCreateDeviceKey(p.keystore, (n) => p.random(n));
        this.fingerprint = fingerprint(x25519Public(this.devicePrivate));
        const manager = new HostManager({
          platform: p,
          store: p.hostStore,
          devicePrivate: this.devicePrivate,
          client: p.client,
        });
        this._manager = manager;
        await manager.start();
      }
      this._push = new PushSync({ manager: this._manager, push: p.push, keystore: p.keystore, random: (n) => p.random(n) });
      this.offs.push(this._manager.subscribe(() => this.onHostsChanged()));
      this.offs.push(p.lifecycle.onVisible(() => void this.housekeeping()));
      void this._push.start();
      this.phase.set('ready');
      void this.housekeeping();
    } catch (e) {
      this.error = (e as Error).message;
      this.phase.set('error');
    }
  }

  stop(): void {
    this.stopped = true;
    for (const h of this.deliveryTimers) this.platform.clock.clearTimeout(h);
    this.deliveryTimers.clear();
    this.offs.forEach((f) => f());
    this.offs = [];
    this._push?.stop();
    this._manager?.stop();
  }

  /** This device's key fingerprint (`abcd-efgh`), shown during pairing. */
  deviceFingerprint(): string | null {
    return this.fingerprint;
  }

  // ---- hosts -------------------------------------------------------------------------------

  conn(hostId: string): HostConnectionApi | undefined {
    return this._manager?.get(hostId);
  }

  async pair(link: PairingLink, deviceName: string, onPending: (fp: string) => void): Promise<HostRecord> {
    if (this.platform.engine) return this.platform.engine.pair(link, deviceName, onPending);
    if (!this.devicePrivate || !(this._manager instanceof HostManager)) throw new Error('app not started');
    // Invitations get their own key: the host keeps them apart from this device's own pairing.
    const p = this.platform;
    const { devicePrivate, keyName } = await pairingKey(p.keystore, link, this.devicePrivate, (n) => p.random(n));
    let record: HostRecord;
    try {
      record = await corePair({ link, platform: p, devicePrivate, keyName, deviceName, onPending });
    } catch (e) {
      if (keyName) await p.keystore.delete(keyName).catch(() => {});
      throw e;
    }
    const old = this._manager.get(record.host_id)?.getSnapshot().record.key;
    await this._manager.add(record);
    if (old && old !== record.key) await p.keystore.delete(old).catch(() => {});
    return record;
  }

  async forgetHost(hostId: string): Promise<void> {
    const c = this.conn(hostId);
    if (c && c.getSnapshot().status === 'online') {
      await c.request('push.unsubscribe', {}).catch(() => {});
    }
    const key = c?.getSnapshot().record.key;
    await this.manager.remove(hostId);
    // An invitation's own key is useless once its record is gone (the engine drops its own).
    if (key && !this.platform.engine) await this.platform.keystore.delete(key).catch(() => {});
    if (this._push) {
      await this._push.forgetHost(hostId);
      // §8.1: the forgotten host keeps the old VAPID key; rotate so it can no longer push here.
      if (this._push.state === 'on') await this._push.rotate().catch(() => {});
    }
    void this.housekeeping();
  }

  // ---- seen/finished -----------------------------------------------------------------------

  private onHostsChanged(): void {
    // Baseline done_rev for runs we have never seen so only *new* completions count as "finished".
    const seen = this.prefs.get().seenDone;
    const add: Record<string, number> = {};
    for (const h of this.manager.getSnapshot()) {
      for (const r of h.dashboard?.runs ?? []) {
        const k = runKey(h.record.host_id, r.id);
        if (seen[k] === undefined) add[k] = r.done_rev;
      }
    }
    if (Object.keys(add).length) this.prefs.patch({ seenDone: { ...seen, ...add } });
    this.platform.notifications?.setBadge(badgeCount(this.manager.getSnapshot()));
  }

  /** Foreground reconciliation (§7.8): close stale notifications, refresh the badge. */
  async housekeeping(): Promise<void> {
    const n = this.platform.notifications;
    if (!n || !this._manager) return;
    const hosts = this._manager.getSnapshot();
    n.setBadge(badgeCount(hosts));
    try {
      const tags = await n.shownTags();
      const close = staleTags(tags, hosts);
      if (close.length) await n.close(close);
    } catch {
      // best effort
    }
  }

  // ---- feedback ----------------------------------------------------------------------------

  haptic(kind: HapticKind): void {
    if (this.prefs.get().haptics) this.platform.haptics?.(kind);
  }

  toast(text: string, tone: Toast['tone'] = 'info', ms = 3000): void {
    const id = ++this.toastSeq;
    this.toasts.update((l) => [...l.slice(-2), { id, text, tone }]);
    this.platform.clock.setTimeout(() => this.toasts.update((l) => l.filter((x) => x.id !== id)), ms);
  }

  // ---- answers -----------------------------------------------------------------------------

  async answer(hostId: string, it: Interaction, params: AnswerParams, label: string): Promise<void> {
    const key = `${hostId}/${it.id}`;
    const conn = this.conn(hostId);
    const now = () => this.platform.clock.now();
    this.answers.set(key, { phase: 'sending', label, at: now() });
    try {
      if (!conn) throw new Error('host not paired');
      const r = await conn.request('interaction.answer', answerParams(it, params));
      const channel = r && typeof r.delivery === 'object' && r.delivery ? r.delivery.channel : undefined;
      this.answers.set(key, { phase: 'sent', label, channel, at: now() });
      this.haptic('success');
      void conn.refresh().catch(() => {});
      void this.followDelivery(hostId, it.id);
    } catch (e) {
      const cls = classifyError(e);
      this.haptic('error');
      if (isPickerChanged(e)) {
        // The dialog moved on under the card: show the fresh one and say so, no error state.
        this.answers.set(key, null);
        this.toast(t.picker.changed, 'info', 4000);
        void conn?.refresh().catch(() => {});
      } else if (cls === 'stale') {
        this.answers.set(key, { phase: 'stale', label, at: now(), error: errorMessage(e) });
        const fresh = staleInteraction(e);
        void conn?.refresh().catch(() => {});
        if (fresh && fresh.status !== 'open') this.answers.set(key, null);
      } else if (cls === 'unknown') {
        this.answers.set(key, { phase: 'unknown', label, at: now() });
        void conn?.refresh().catch(() => {});
      } else {
        this.answers.set(key, { phase: 'error', label, at: now(), error: errorMessage(e) });
      }
    }
  }

  /** Poll `interaction.get` until the answer settles (delivered / failed / closed), ≤ ~30 s. */
  private async followDelivery(hostId: string, id: string): Promise<void> {
    const key = `${hostId}/${id}`;
    const terminal = new Set(['delivered', 'failed', 'delivery_unknown', 'superseded', 'resolved_elsewhere']);
    const clock = this.platform.clock;
    for (let i = 0; i < 15; i++) {
      await new Promise((r) => {
        const h = clock.setTimeout(() => {
          this.deliveryTimers.delete(h);
          r(null);
        }, i === 0 ? 400 : 2000);
        this.deliveryTimers.add(h);
      });
      if (this.stopped) return;
      const conn = this.conn(hostId);
      if (!conn || !this.answers.get(key)) return;
      try {
        const r = await conn.request('interaction.get', { interaction: id });
        if (this.stopped) return;
        const live = normalizeInteraction(r.interaction);
        this.rememberFinal(key, live);
        // Re-render cards that read `finals` (the answer store drives the inbox).
        const cur = this.answers.get(key);
        if (cur) this.answers.set(key, { ...cur });
        if (live.status !== 'open' && (terminal.has(live.delivery) || live.delivery === 'decision_recorded')) return;
      } catch {
        return;
      }
    }
  }

  /** Most recent first-class answers only: the map is bounded (oldest evicted). */
  private rememberFinal(key: string, it: Interaction): void {
    this.finals.delete(key);
    this.finals.set(key, it);
    while (this.finals.size > MAX_FINALS) this.finals.delete(this.finals.keys().next().value!);
  }

  /** Clear a card's local state (after a stale refresh the user decides again). */
  resetAnswer(hostId: string, id: string): void {
    this.answers.set(`${hostId}/${id}`, null);
  }

  async answerBatch(batch: Batch, decision: 'allow' | 'deny'): Promise<{ ok: number; total: number }> {
    const conn = this.conn(batch.host_id);
    const label = decision;
    const keys = batch.items.map((it) => `${batch.host_id}/${it.interaction.id}`);
    const now = this.platform.clock.now();
    for (const k of keys) this.answers.set(k, { phase: 'sending', label, at: now });
    try {
      if (!conn) throw new Error('host not paired');
      const r = await conn.request('interaction.answer_batch', batchAnswerParams(batch, decision));
      let ok = 0;
      for (const res of r.results) {
        const k = `${batch.host_id}/${res.interaction}`;
        if (res.ok) {
          ok++;
          const d = res.result?.delivery;
          this.answers.set(k, { phase: 'sent', label, channel: d && typeof d === 'object' ? d.channel : undefined, at: now });
          void this.followDelivery(batch.host_id, res.interaction);
        } else {
          const stale = res.error?.data?.kind === 'stale';
          this.answers.set(k, { phase: stale ? 'stale' : 'error', label, error: res.error?.message, at: now });
        }
      }
      this.haptic(ok === r.results.length ? 'success' : 'warning');
      void conn.refresh().catch(() => {});
      return { ok, total: batch.items.length };
    } catch (e) {
      const cls = classifyError(e);
      for (const k of keys) {
        this.answers.set(k, cls === 'stale' ? null : { phase: cls === 'unknown' ? 'unknown' : 'error', label, error: errorMessage(e), at: now });
      }
      if (cls === 'stale') this.toast(errorMessage(e), 'warn');
      this.haptic('error');
      void conn?.refresh().catch(() => {});
      return { ok: 0, total: batch.items.length };
    }
  }
}
