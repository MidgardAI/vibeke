// Answer flow state for inbox cards (spec 16 §9.2): progress → delivery state from the live
// interaction, `stale` refresh, unknown outcome (never retried, §1.7), answered elsewhere.

import { NotConnectedError, OutcomeUnknownError, RpcError, type Interaction } from '@vibeke/core';
import { t } from '../i18n';

export type LocalPhase = 'sending' | 'sent' | 'stale' | 'unknown' | 'error';

export interface LocalAnswer {
  phase: LocalPhase;
  /** What we sent, for the card's caption. */
  label: string;
  error?: string;
  /** Delivery channel reported by the server (`native`, `keystrokes`, `recorded`). */
  channel?: string;
  at: number;
}

export type DeliveryView =
  | 'sending'
  | 'delivering'
  | 'delivered'
  | 'recorded'
  | 'failed'
  | 'unknown'
  | 'superseded'
  | 'stale'
  | 'error'
  | 'answered_elsewhere';

/** Combine what this device did with the live interaction from the dashboard. */
export function deliveryView(local: LocalAnswer | undefined, live: Interaction | undefined): DeliveryView | null {
  if (!local) {
    if (live && live.status !== 'open') return live.status === 'answered' && live.answered_by?.startsWith('gateway:') ? fromDelivery(live) : 'answered_elsewhere';
    return null;
  }
  switch (local.phase) {
    case 'sending':
      return 'sending';
    case 'stale':
      return 'stale';
    case 'error':
      return 'error';
    case 'unknown':
      return live && live.status !== 'open' ? fromDelivery(live) : 'unknown';
    case 'sent':
      return live ? fromDelivery(live) : local.channel === 'recorded' ? 'recorded' : 'delivering';
  }
}

function fromDelivery(i: Interaction): DeliveryView {
  switch (i.delivery) {
    case 'delivered':
      return 'delivered';
    case 'failed':
      return 'failed';
    case 'delivery_unknown':
      return 'unknown';
    case 'superseded':
      return 'superseded';
    case 'resolved_elsewhere':
      return 'answered_elsewhere';
    case 'decision_recorded':
      return i.status === 'open' ? 'delivering' : 'recorded';
    default:
      return 'delivering';
  }
}

/** Terminal states: the card can leave the inbox after a short beat. */
export const isSettled = (v: DeliveryView | null): boolean =>
  v === 'delivered' || v === 'recorded' || v === 'superseded' || v === 'answered_elsewhere';

/** Needs the user to look at the pane. */
export const needsPane = (v: DeliveryView | null): boolean => v === 'failed' || v === 'unknown';

export type ErrorClass = 'stale' | 'unknown' | 'offline' | 'forbidden' | 'unsupported' | 'error';

export function classifyError(e: unknown): ErrorClass {
  if (e instanceof OutcomeUnknownError) return 'unknown';
  if (e instanceof NotConnectedError) return 'offline';
  if (e instanceof RpcError) {
    if (e.kind === 'stale' || e.kind === 'conflict') return 'stale';
    if (e.kind === 'forbidden' || e.kind === 'permission_denied') return 'forbidden';
    if (e.kind === 'unsupported') return 'unsupported';
  }
  return 'error';
}

/** Interaction carried in a `stale` error's details (gateway returns the fresh one). */
export function staleInteraction(e: unknown): Interaction | null {
  if (!(e instanceof RpcError)) return null;
  const d = e.data?.details as { interaction?: unknown } | undefined;
  const it = d?.interaction;
  return it && typeof it === 'object' && 'id' in it ? (it as Interaction) : null;
}

/** Gateway error messages that are codes, not sentences. */
const KNOWN_MESSAGES = new Map<string, () => string>([['not_a_repo', () => t.changes.notRepo]]);

/** Human message for an error. */
export function errorMessage(e: unknown): string {
  const m = e instanceof RpcError ? (e.data?.kind && e.message.includes(':') ? e.message.split(': ').slice(1).join(': ') : e.message) : e instanceof Error ? e.message : String(e);
  return KNOWN_MESSAGES.get(m)?.() ?? m;
}

export class AnswerStore {
  private m = new Map<string, LocalAnswer>();
  private listeners = new Set<() => void>();
  private version = 0;

  get(key: string): LocalAnswer | undefined {
    return this.m.get(key);
  }
  set(key: string, v: LocalAnswer | null): void {
    if (v) this.m.set(key, v);
    else this.m.delete(key);
    this.version++;
    for (const cb of [...this.listeners]) cb();
  }
  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };
  getSnapshot = (): number => this.version;
}
