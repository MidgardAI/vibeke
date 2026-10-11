import { describe, expect, test } from 'bun:test';
import { confirmText } from '../src/components/interaction-card';
import { t } from '../src/i18n';
import { interaction } from './fixtures';

const withRisk = (risk: 'low' | 'medium' | 'high' | 'unknown') => interaction({ action: { ...interaction().action!, risk } });

describe('confirmation text', () => {
  test('allow always on low or medium risk says what it grants, not "high risk"', () => {
    for (const r of ['low', 'medium'] as const) {
      const s = confirmText('allow_always', withRisk(r));
      expect(s).toBe(t.inbox.confirmAlways('`pnpm test`'));
      expect(s).not.toContain('high or unknown');
    }
  });
  test('allow always on high risk names both', () => {
    expect(confirmText('allow_always', withRisk('high'))).toBe(t.inbox.confirmAlwaysHigh('`pnpm test`'));
  });
  test('allow on high risk keeps the risk text', () => {
    expect(confirmText('allow', withRisk('unknown'))).toBe(t.inbox.confirmHigh('`pnpm test`'));
  });
});

import { RpcError } from '@vibeke/core';
import { closeReasonFromEvent, itemChanged } from '../src/app/model';
import { closedView, deliveryView, isSettled } from '../src/lib/answer';

describe('closed without an answer from this device', () => {
  const ev = (type: string, data: Record<string, unknown> = {}) => ({ seq: 1, ts: 0, type, subject: { interaction: 'i1' }, data });
  test('events say how it closed', () => {
    expect(closeReasonFromEvent(ev('interaction.cancelled', { reason: 'no dialog', closed_by: 'heuristic' }))).toEqual({ interaction: 'i1', reason: 'heuristic' });
    expect(closeReasonFromEvent(ev('interaction.cancelled', { reason: 'dismissed' }))).toEqual({ interaction: 'i1', reason: 'dismissed' });
    expect(closeReasonFromEvent(ev('interaction.cancelled', { reason: 'gate dropped' }))).toEqual({ interaction: 'i1', reason: 'cancelled' });
    expect(closeReasonFromEvent(ev('interaction.expired'))).toEqual({ interaction: 'i1', reason: 'expired' });
    expect(closeReasonFromEvent(ev('interaction.resolved_elsewhere'))).toEqual({ interaction: 'i1', reason: 'elsewhere' });
    expect(closeReasonFromEvent(ev('interaction.opened'))).toBeNull();
  });
  test('a dismissed or auto-closed card never says "answered in terminal"', () => {
    expect(closedView('dismissed', undefined)).toBe('dismissed');
    expect(closedView('heuristic', undefined)).toBe('closed_auto');
    expect(closedView(undefined, interaction({ status: 'cancelled' }))).toBe('closed');
    expect(closedView(undefined, interaction({ status: 'expired' }))).toBe('expired');
    expect(closedView('elsewhere', undefined)).toBe('answered_elsewhere');
    expect(deliveryView(undefined, interaction({ status: 'cancelled' }), 'heuristic')).toBe('closed_auto');
    expect(isSettled('dismissed')).toBe(true);
  });
  test('a changed request is recognised', () => {
    expect(itemChanged(new RpcError('attention.update', { code: -32009, message: 'This request changed', data: { kind: 'conflict', details: { reason: 'item_changed' } } }))).toBe(true);
    expect(itemChanged(new RpcError('x', { code: -32009, message: 'conflict', data: { kind: 'conflict' } }))).toBe(false);
  });
});
