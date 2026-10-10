import { describe, expect, test } from 'bun:test';
import type { AppEvent } from '@vibeke/core';
import { EVENT, INVOKE, RENDERER_METHODS, type Bridge } from '../src/shared/contract';
import { EventSubscriptions, MAX_EVENT_BYTES, forwardableEvent, isForwardedEventType, parseHostEvent } from '../src/shared/host-events';
import { RemoteManager } from '../src/renderer/remote';
import * as v from '../src/main/validate';

const ev = (type: string, extra: Partial<AppEvent> = {}): AppEvent => ({ seq: 7, ts: 1, type, subject: { run: 'r1', pane: 'p1' }, data: { facet: 'execution' }, ...extra });

describe('host event forwarding (main → renderer)', () => {
  test('only the types the UI needs cross the bridge', () => {
    for (const t of ['agent.turn_started', 'agent.state_changed', 'agent.usage', 'interaction.opened', 'notification.created', 'task.updated', 'preview.up', 'tab.created', 'pane.closed', 'handoff.job', 'handoff.incoming', 'handoff.updated', 'handoff.expired', 'screenshot.captured', 'screenshot.deleted', 'auth.approval_requested', 'auth.approval_granted', 'auth.approval_denied', 'auth.approval_withdrawn'])
      expect(isForwardedEventType(t)).toBe(true);
    for (const t of ['auth.elevate_requested', 'auth.elevate_granted', 'auth.revoked', 'notification.read', 'session.started', 'workspace.created', 'device.revoked', 'push.sent', 'agent', '', 'Agent.x', 'agent.<script>', 1, null])
      expect(isForwardedEventType(t)).toBe(false);
  });

  test('events are copied as plain JSON; malformed or oversized ones are dropped', () => {
    const out = forwardableEvent(ev('agent.turn_completed'));
    expect(out).toEqual(ev('agent.turn_completed'));
    expect(forwardableEvent(ev('session.started'))).toBeNull();
    expect(forwardableEvent({ ...ev('agent.usage'), seq: -1 })).toBeNull();
    expect(forwardableEvent({ ...ev('agent.usage'), seq: '7' })).toBeNull();
    expect(forwardableEvent({ ...ev('agent.usage'), subject: { run: 5 } })).toBeNull();
    expect(forwardableEvent({ ...ev('agent.usage'), subject: { run: 'r1', pane: undefined } })?.subject).toEqual({ run: 'r1' });
    expect(forwardableEvent({ ...ev('agent.usage'), data: [] })).toBeNull();
    expect(forwardableEvent({ ...ev('agent.usage'), data: { big: 'x'.repeat(MAX_EVENT_BYTES) } })).toBeNull();
    expect(forwardableEvent({ ...ev('agent.usage'), data: undefined })?.data).toEqual({});
    expect(forwardableEvent([])).toBeNull();
    expect(forwardableEvent(null)).toBeNull();
  });

  test('renderer validates every payload', () => {
    expect(parseHostEvent({ hostId: 'h1', event: ev('agent.turn_started') })).toEqual({ hostId: 'h1', event: ev('agent.turn_started') });
    expect(parseHostEvent({ hostId: '../h', event: ev('agent.turn_started') })).toBeNull();
    expect(parseHostEvent({ hostId: 'h1', event: ev('device.revoked') })).toBeNull();
    expect(parseHostEvent({ hostId: 'h1' })).toBeNull();
    expect(parseHostEvent('x')).toBeNull();
  });

  test('subscriptions are per window and host; a new document clears them', () => {
    const subs = new EventSubscriptions<object>();
    const a = {};
    const b = {};
    subs.set(a, 'h1', true);
    expect(subs.wants(a, 'h1')).toBe(true);
    expect(subs.wants(a, 'h2')).toBe(false);
    expect(subs.wants(b, 'h1')).toBe(false);
    subs.set(b, 'h1', false);
    expect(subs.wants(b, 'h1')).toBe(false);
    subs.set(a, 'h2', true);
    subs.set(a, 'h1', false);
    expect(subs.wants(a, 'h1')).toBe(false);
    expect(subs.wants(a, 'h2')).toBe(true);
    subs.clear(a);
    expect(subs.wants(a, 'h2')).toBe(false);
  });

  test('the host-events toggle validates its arguments', () => {
    expect(v.flag(true, 'f')).toBe(true);
    expect(v.flag(false, 'f')).toBe(false);
    for (const bad of ['true', 1, null, undefined]) expect(() => v.flag(bad, 'f')).toThrow(v.IpcValidationError);
  });

  test('RemoteManager: subscribes once per host, delivers validated events, unsubscribes on the last listener', async () => {
    const invokes: unknown[][] = [];
    const handlers = new Map<string, (p: unknown) => void>();
    const bridge: Bridge = {
      invoke: async (channel, ...args) => {
        invokes.push([channel, ...args]);
        return { ok: true, value: null };
      },
      on: (channel, cb) => {
        handlers.set(channel, cb);
        return () => handlers.delete(channel);
      },
    };
    const m = new RemoteManager(bridge);
    const got: string[] = [];
    const off1 = m.subscribeEvents('h1', (e) => got.push(`1:${e.type}`));
    const off2 = m.subscribeEvents('h1', (e) => got.push(`2:${e.type}`));
    m.subscribeEvents('h2', (e) => got.push(`h2:${e.type}`));
    expect(invokes).toEqual([
      [INVOKE.hostEvents, 'h1', true],
      [INVOKE.hostEvents, 'h2', true],
    ]);
    const send = handlers.get(EVENT.hostEvent)!;
    send({ hostId: 'h1', event: ev('agent.turn_started') });
    send({ hostId: 'h1', event: ev('device.revoked') }); // not forwardable: dropped
    send({ hostId: 'h1', event: { ...ev('agent.usage'), seq: 'x' } }); // malformed: dropped
    send({ hostId: 'h3', event: ev('agent.usage') }); // nobody listens
    expect(got).toEqual(['1:agent.turn_started', '2:agent.turn_started']);
    off1();
    expect(invokes.length).toBe(2);
    off2();
    expect(invokes[2]).toEqual([INVOKE.hostEvents, 'h1', false]);
    off2(); // idempotent
    expect(invokes.length).toBe(3);
    m.stop();
    expect(handlers.has(EVENT.hostEvent)).toBe(false);
  });
});

describe('renderer method allow-list', () => {
  test('tab.rename / tab.close / tab.focus / preview.open pass the bridge validator', () => {
    for (const m of ['tab.rename', 'tab.close', 'tab.focus', 'preview.open', 'tab.create'] as const) {
      expect(RENDERER_METHODS).toContain(m);
      expect(v.method(m)).toBe(m);
    }
    // Connection management and Web Push stay with the engine.
    for (const m of ['events.subscribe', 'push.subscribe', 'push.test', 'hello']) expect(() => v.method(m)).toThrow(v.IpcValidationError);
  });

  test('the handoff send, job and incoming methods pass the bridge validator', () => {
    for (const m of ['handoff.send', 'handoff.jobs', 'handoff.cancel', 'handoff.peers', 'handoff.prefs', 'handoff.incoming.list', 'handoff.accept', 'peer.invite', 'peer.redeem', 'share.list', 'share.revoke'] as const) {
      expect(RENDERER_METHODS).toContain(m);
      expect(v.method(m)).toBe(m);
    }
  });

  test('approval review methods pass the bridge validator', () => {
    for (const m of ['auth.list', 'auth.approve.decide'] as const) {
      expect(RENDERER_METHODS).toContain(m);
      expect(v.method(m)).toBe(m);
    }
    // A pane's own side of the protocol and elevation never cross the bridge.
    for (const m of ['auth.approve', 'auth.approve.withdraw', 'auth.elevate.decide']) expect(() => v.method(m)).toThrow(v.IpcValidationError);
  });

  test('the retired courier methods are refused', () => {
    for (const m of ['handoff.export', 'handoff.read', 'handoff.begin', 'handoff.write', 'handoff.finish', 'handoff.discard']) {
      expect(RENDERER_METHODS as readonly string[]).not.toContain(m);
      expect(() => v.method(m)).toThrow(v.IpcValidationError);
    }
  });
});
