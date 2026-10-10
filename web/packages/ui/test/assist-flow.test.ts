import { describe, expect, test } from 'bun:test';
import { RpcError, type AssistantRequest } from '@vibeke/core';
import { assistEligible, ReplyCache, assistReady, turnStamp } from '../src/lib/assist-access';
import { AssistFlow, assistError, costText, parseReplies, stateFromGenerate, stateFromRequest, summaryItems, IDLE_ASSIST, type AssistConn } from '../src/lib/assist-flow';

const req = (p: Partial<AssistantRequest> = {}): AssistantRequest => ({ id: 'q1', operation: 'reply_suggestions', state: 'queued', ...p });
const preview = { digest: 'dg', model: 'm', estimated_max_cost_usd: 0.004 };

describe('assistant states', () => {
  test('a request that needs confirmation shows the preview and does not run', () => {
    const s = stateFromGenerate({ request: req({ state: 'awaiting_confirmation' }), preview, requires_confirmation: true });
    expect(s.phase).toBe('confirm');
    expect(s.preview?.digest).toBe('dg');
  });
  test('confirmation without a preview stops instead of confirming blind', () => {
    const s = stateFromGenerate({ request: req({ state: 'awaiting_confirmation' }), requires_confirmation: true });
    expect(s.phase).toBe('failed');
  });
  test('a host that auto-sends goes straight to running; a cached answer is done', () => {
    expect(stateFromGenerate({ request: req({ state: 'running' }) }).phase).toBe('running');
    const done = stateFromGenerate({ request: req({ state: 'done', output: { replies: ['ok'] } }), cached: true });
    expect(done).toMatchObject({ phase: 'done', cached: true, output: { replies: ['ok'] } });
  });
  test('failed and cancelled requests read as failures', () => {
    expect(stateFromRequest(req({ state: 'failed', error: { message: 'quota' } })).error).toBe('quota');
    expect(stateFromRequest(req({ state: 'cancelled' })).phase).toBe('failed');
  });
  test('errors: consent, unsupported, other', () => {
    const rpc = (kind: string, message = 'x', code = -32000) => new RpcError('assistant.generate', { code, message, data: { kind } });
    expect(assistError(rpc('forbidden')).consent).toBe(true);
    expect(assistError(rpc('invalid_params', 'assistant.generate: consent required')).consent).toBe(true);
    expect(assistError(rpc('method_not_found', 'nope', -32601)).consent).toBe(false);
    expect(assistError(new Error('boom')).message).toBe('boom');
  });
  test('costText', () => {
    expect(costText(null)).toBeNull();
    expect(costText({ digest: 'd', estimated_max_cost_usd: 0.004 })).toContain('< $0.01');
    expect(costText({ digest: 'd', estimated_max_cost_usd: 0.25 })).toContain('$0.25');
  });
});

function fakeConn(script: Record<string, (p: any) => unknown>) {
  const calls: { method: string; params: any }[] = [];
  const conn = {
    request: async (method: string, params: any) => {
      calls.push({ method, params });
      const f = script[method];
      if (!f) throw new Error(`unexpected ${method}`);
      return f(params);
    },
  } as unknown as AssistConn;
  return { conn, calls };
}

describe('assistant flow', () => {
  test('confirm is never sent until the user confirms', async () => {
    let polls = 0;
    const { conn, calls } = fakeConn({
      'assistant.generate': () => ({ request: req({ state: 'awaiting_confirmation' }), preview, requires_confirmation: true }),
      'assistant.confirm': () => ({ request: req({ state: 'running' }) }),
      'assistant.get': () => ({ request: req({ state: ++polls < 2 ? 'running' : 'done', output: { replies: ['Yes', 'No'] } }) }),
    });
    const flow = new AssistFlow(() => conn, { sleep: async () => {}, pollMs: 0 });
    await flow.start({ operation: 'reply_suggestions', pane: 'p1' });
    expect(flow.state.phase).toBe('confirm');
    expect(calls.map((c) => c.method)).toEqual(['assistant.generate']);
    await flow.confirm();
    expect(calls.map((c) => c.method)).toEqual(['assistant.generate', 'assistant.confirm', 'assistant.get', 'assistant.get']);
    expect(calls[1]!.params).toEqual({ request: 'q1', preview_digest: 'dg' });
    expect(flow.state.phase).toBe('done');
    expect(parseReplies(flow.state.output)).toEqual(['Yes', 'No']);
  });

  test('cancel while confirming asks the host to cancel and goes idle', async () => {
    const { conn, calls } = fakeConn({
      'assistant.generate': () => ({ request: req({ state: 'awaiting_confirmation' }), preview, requires_confirmation: true }),
      'assistant.cancel': () => ({ request: req({ state: 'cancelled' }) }),
    });
    const flow = new AssistFlow(() => conn, { sleep: async () => {} });
    await flow.start({ operation: 'briefing' });
    flow.cancel();
    expect(flow.state).toEqual(IDLE_ASSIST);
    expect(calls.at(-1)!.method).toBe('assistant.cancel');
  });

  test('a permission error asks for consent at the desk', async () => {
    const { conn } = fakeConn({
      'assistant.generate': () => {
        throw new RpcError('assistant.generate', { code: -32000, message: 'consent missing', data: { kind: 'forbidden' } });
      },
    });
    const flow = new AssistFlow(() => conn);
    await flow.start({ operation: 'briefing' });
    expect(flow.state).toMatchObject({ phase: 'failed', consent: true });
  });

  test('polling gives up after the time limit', async () => {
    let now = 0;
    const { conn } = fakeConn({
      'assistant.generate': () => ({ request: req({ state: 'running' }) }),
      'assistant.get': () => ({ request: req({ state: 'running' }) }),
    });
    const flow = new AssistFlow(() => conn, { sleep: async () => void (now += 1000), pollMs: 1000, maxMs: 3000, now: () => now });
    await flow.start({ operation: 'briefing' });
    expect(flow.state.phase).toBe('failed');
  });

  test('a late answer for a cancelled start is dropped', async () => {
    let release!: () => void;
    const gate = new Promise<void>((r) => (release = r));
    const { conn } = fakeConn({
      'assistant.generate': async () => {
        await gate;
        return { request: req({ state: 'done', output: { replies: ['late'] } }) };
      },
    });
    const flow = new AssistFlow(() => conn);
    const p = flow.start({ operation: 'reply_suggestions', pane: 'p1' });
    flow.reset();
    release();
    await p;
    expect(flow.state.phase).toBe('idle');
  });
});

describe('outputs', () => {
  test('parseReplies keeps up to five distinct one-line strings', () => {
    expect(parseReplies(null)).toEqual([]);
    expect(parseReplies({ replies: ['a\nb', 'a b', '', 3, 'c', 'd', 'e', 'f', 'g'] })).toEqual(['a b', 'c', 'd', 'e', 'f']);
  });
  test('summaryItems reads briefings and plain summaries', () => {
    expect(summaryItems({ items: [{ text: 'Tests fail', urgency: 'now' }, { text: 'Docs updated', urgency: 'x' }], coverage: {} })).toEqual([{ text: 'Tests fail', urgency: 'now' }, { text: 'Docs updated' }]);
    expect(summaryItems({ summary: ' Done. ' })).toEqual([{ text: 'Done.' }]);
    expect(summaryItems({ other: 1 })).toEqual([]);
  });
});

describe('access', () => {
  const info = (p: object = {}) => ({ scope: 'full' as const, features: ['catch_up'], ...p });
  const record = { scope: 'full' as const };
  test('only unlimited full devices on a gateway with catch_up', () => {
    expect(assistEligible({ info: info(), record })).toBe(true);
    expect(assistEligible({ info: info({ features: [] }), record })).toBe(false);
    expect(assistEligible({ info: info({ scope: 'approve' }), record })).toBe(false);
    expect(assistEligible({ info: info({ limit: { pane: 'p1' } }), record })).toBe(false);
    expect(assistEligible({ info: info({ kind: 'share' }), record })).toBe(false);
    expect(assistEligible({ info: null, record })).toBe(false);
  });
  test('assistReady needs enabled and configured', () => {
    expect(assistReady({ enabled: true, configured: true })).toBe(true);
    expect(assistReady({ enabled: true, configured: false })).toBe(false);
    expect(assistReady(null)).toBe(false);
  });
  test('reply cache lasts until the next turn', () => {
    const c = new ReplyCache();
    const s1 = turnStamp({ id: 'r1', turns_completed: 2, done_rev: 2 });
    c.set('h/p', s1, ['ok']);
    expect(c.get('h/p', s1)).toEqual(['ok']);
    expect(c.get('h/p', turnStamp({ id: 'r1', turns_completed: 3, done_rev: 3 }))).toBeNull();
  });
});
