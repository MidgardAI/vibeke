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
