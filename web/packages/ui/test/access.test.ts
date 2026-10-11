import { describe, expect, test } from 'bun:test';
import { configErrorFromDashboard, configErrorFromEvent } from '../src/app/model';
import { t } from '../src/i18n';
import { TICKET_WARN_MS, alsoText, parseAlso, ticketDaysLeft } from '../src/lib/access';

const DAY = 86_400_000;

describe('relay ticket warning', () => {
  const now = 1_800_000_000_000;
  test('warns within seven days, not before, not after expiry', () => {
    expect(ticketDaysLeft({ ticket: 'x', ticket_exp: (now + 3 * DAY) / 1000 }, now)).toBe(3);
    expect(ticketDaysLeft({ ticket: 'x', ticket_exp: (now + 1000) / 1000 }, now)).toBe(1);
    expect(ticketDaysLeft({ ticket: 'x', ticket_exp: (now + TICKET_WARN_MS + DAY) / 1000 }, now)).toBeNull();
    expect(ticketDaysLeft({ ticket: 'x', ticket_exp: (now - 1000) / 1000 }, now)).toBeNull();
    expect(ticketDaysLeft({ ticket_exp: (now + DAY) / 1000 }, now)).toBeNull();
  });
});

describe('revoke summary', () => {
  test('names what else goes', () => {
    expect(alsoText(parseAlso({ shares: 2, hosts: 0, invitations: 1, peers: 0 }), t.settings.also)).toBe('This also removes 2 shares and 1 invitation.');
    expect(alsoText(parseAlso({ shares: 1, hosts: 1, invitations: 0, peers: 1 }), t.settings.also)).toBe('This also removes 1 share and 2 linked hosts.');
    expect(alsoText(parseAlso({ shares: 0 }), t.settings.also)).toBeNull();
    expect(alsoText(parseAlso(null), t.settings.also)).toBeNull();
  });
});

describe('config load errors', () => {
  const ev = (type: string, data: Record<string, unknown> = {}) => ({ seq: 1, ts: 5, type, subject: {}, data });
  test('a rejection lists its first problems; a reload clears it', () => {
    expect(configErrorFromEvent(ev('session.config_rejected', { errors: [{ line: 3, message: 'unknown key' }, { message: 'bad value' }] }))).toEqual({
      errors: ['line 3: unknown key', 'bad value'],
      at: 5,
    });
    expect(configErrorFromEvent(ev('session.config_reloaded'))).toBeNull();
    expect(configErrorFromEvent(ev('pane.created'))).toBeUndefined();
  });
  test('a dashboard seeds the banner for an app that connects after the event', () => {
    const base = { at: 1, session: 's', machine: 'm', workspaces: [], tabs: [], panes: [], runs: [], interactions: [], tasks: [], notifications_unread: 0 };
    expect(configErrorFromDashboard({ ...base, config_error: { path: '/c/config.toml', errors: [{ line: 2, message: 'unknown key' }] } }, 9)).toEqual({
      errors: ['line 2: unknown key'],
      at: 9,
    });
    expect(configErrorFromDashboard({ ...base, config_error: null }, 9)).toBeNull();
    expect(configErrorFromDashboard(base, 9)).toBeUndefined();
    expect(configErrorFromDashboard(null, 9)).toBeUndefined();
  });
});

describe('access levels', () => {
  test('each level says what it allows', () => {
    expect(t.access.hints.full).toContain('any command');
    expect(t.access.hints.approve).toContain('always allow');
    expect(t.access.hints.view).toContain('transcripts');
  });
});
