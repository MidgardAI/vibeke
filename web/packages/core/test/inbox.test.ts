import { describe, expect, test } from 'bun:test';
import {
  answerParams,
  batchAnswerParams,
  batchFingerprint,
  groupBatches,
  rankInbox,
  rankRuns,
  swipeAllowed,
  type InboxItem,
} from '../src/inbox';
import { normalizeInteraction, type AgentRun, type Interaction, type Risk } from '../src/model';

let n = 0;
function ix(p: Partial<Interaction> & { risk?: Risk; command?: string | null; paths?: string[] } = {}): Interaction {
  const { risk = 'low', command = 'pnpm test', paths = [], ...rest } = p;
  return {
    id: `i${++n}`,
    handle: `i${n}`,
    run: 'r1',
    pane: 'p1',
    kind: 'approval',
    status: 'open',
    title: 't',
    body_md: null,
    action: { tool: 'Bash', summary: 's', command, paths, diff: null, risk, risk_reasons: [] },
    questions: [],
    plan_md: null,
    answer_channel: 'native',
    native_ref: null,
    source: 'structured',
    confidence: 1,
    answerable: true,
    gate: true,
    decision_rev: 1,
    delivery: 'none',
    delivery_error: null,
    answer: null,
    answered_by: null,
    opened_at_ms: 1000,
    answered_at_ms: null,
    ...rest,
  };
}

function run(id: string, value: AgentRun['execution']['value'], p: Partial<AgentRun> = {}): AgentRun {
  return {
    id,
    handle: id,
    name: null,
    pane: `p-${id}`,
    harness: 'claude',
    harness_version: null,
    integration: 'hooks',
    harness_session_id: null,
    transcript_path: null,
    resume_argv: [],
    cwd: '/repo',
    model: null,
    task: null,
    execution: { value, since_ms: 0, source: 'structured', confidence: 1, detail: null },
    health: 'healthy',
    yolo: false,
    permission_mode: null,
    last_message: null,
    last_tool: null,
    turns_completed: 0,
    done_rev: 0,
    started_at_ms: 0,
    ended_at_ms: null,
    capabilities: [],
    ...p,
  };
}

const item = (i: Interaction, r = run(i.run, 'working'), host = 'h1'): InboxItem => ({ host_id: host, interaction: i, run: r });

describe('ranking', () => {
  test('risk first (high, unknown, medium, low), then longest wait', () => {
    const low = ix({ risk: 'low', opened_at_ms: 1 });
    const medOld = ix({ risk: 'medium', opened_at_ms: 5 });
    const medNew = ix({ risk: 'medium', opened_at_ms: 9 });
    const high = ix({ risk: 'high', opened_at_ms: 100 });
    const question = ix({ kind: 'question', action: null, opened_at_ms: 50 });
    const answered = ix({ risk: 'high', status: 'answered' });
    const ranked = rankInbox([low, medNew, answered, high, question, medOld].map((i) => item(i)));
    expect(ranked.map((r) => r.interaction.id)).toEqual([high.id, question.id, medOld.id, medNew.id, low.id]);
  });

  test('runs: interaction, needs input, working, idle', () => {
    const runs = [
      run('idle', 'idle', { done_rev: 1 }),
      run('work', 'working'),
      run('done', 'idle', { done_rev: 3 }),
      run('err', 'rate_limited'),
      run('ask', 'working'),
      run('askHigh', 'working'),
    ];
    const ints = [ix({ run: 'ask', risk: 'low' }), ix({ run: 'askHigh', risk: 'high' })];
    const ranked = rankRuns(runs, ints, { seenDoneRev: (r) => (r.id === 'done' ? 2 : r.done_rev) });
    expect(ranked.map((r) => [r.run.id, r.attention])).toEqual([
      ['askHigh', 'interaction'],
      ['ask', 'interaction'],
      ['done', 'needs_input'],
      ['err', 'needs_input'],
      ['work', 'working'],
      ['idle', 'idle'],
    ]);
    expect(ranked[0]!.top?.action?.risk).toBe('high');
  });
});

describe('batching', () => {
  test('eligibility', () => {
    expect(swipeAllowed(ix({ risk: 'low' }))).toBe(true);
    expect(swipeAllowed(ix({ risk: 'medium' }))).toBe(true);
    expect(swipeAllowed(ix({ risk: 'high' }))).toBe(false);
    expect(swipeAllowed(ix({ risk: 'unknown' }))).toBe(false);
    expect(swipeAllowed(ix({ answerable: false }))).toBe(false);
    expect(swipeAllowed(ix({ status: 'answered' }))).toBe(false);
    expect(swipeAllowed(ix({ kind: 'question' }))).toBe(false);
  });

  test('fingerprint uses the exact command and sorts paths', () => {
    const ctx = { harness: 'claude', repoRoot: '/repo' };
    expect(batchFingerprint(ix({ command: 'pnpm test' }), ctx)).toBe(batchFingerprint(ix({ command: 'pnpm test' }), ctx));
    // Whitespace is significant: a newline turns one command into two.
    expect(batchFingerprint(ix({ command: 'echo harmless rm notes.txt' }), ctx)).not.toBe(
      batchFingerprint(ix({ command: 'echo harmless\nrm notes.txt' }), ctx),
    );
    expect(batchFingerprint(ix({ command: '  pnpm   test ' }), ctx)).not.toBe(batchFingerprint(ix({ command: 'pnpm test' }), ctx));
    expect(batchFingerprint(ix({ command: null, paths: ['b', 'a'] }), ctx)).toBe(
      batchFingerprint(ix({ command: null, paths: ['a', 'b'] }), ctx),
    );
    expect(batchFingerprint(ix(), ctx)).not.toBe(batchFingerprint(ix(), { ...ctx, repoRoot: '/other' }));
    expect(batchFingerprint(ix(), ctx)).not.toBe(batchFingerprint(ix(), { ...ctx, harness: 'codex' }));
    expect(batchFingerprint(ix({ risk: 'high' }), ctx)).toBeNull();
  });

  test('groups by host + fingerprint, singletons dropped, high risk excluded', () => {
    const a = item(ix({ run: 'r1' }), run('r1', 'working'));
    const b = item(ix({ run: 'r2', risk: 'medium' }), run('r2', 'working'));
    const c = item(ix({ run: 'r3', command: 'pnpm build' }), run('r3', 'working'));
    const d = item(ix({ run: 'r4', risk: 'high' }), run('r4', 'working'));
    const e = item(ix({ run: 'r5' }), run('r5', 'working', { cwd: '/elsewhere' }));
    const f = item(ix({ run: 'r6' }), run('r6', 'working'), 'h2');
    const batches = groupBatches([a, b, c, d, e, f]);
    expect(batches.length).toBe(1);
    expect(batches[0]!.items.map((x) => x.interaction.id).sort()).toEqual([a.interaction.id, b.interaction.id].sort());
    expect(batches[0]!.risk).toBe('medium');
    const params = batchAnswerParams(batches[0]!, 'allow');
    expect(params.decision).toBe('allow');
    expect(params.items[0]).toHaveProperty('decision_rev', 1);
    expect(() => batchAnswerParams(batches[0]!, 'allow_always' as 'allow')).toThrow();
  });

  test('commands differing only in whitespace never group (newline = second command)', () => {
    const a = item(ix({ run: 'r1', command: 'echo harmless rm notes.txt' }), run('r1', 'working'));
    const b = item(ix({ run: 'r2', command: 'echo harmless\nrm notes.txt' }), run('r2', 'working'));
    const c = item(ix({ run: 'r3', command: 'echo harmless  rm notes.txt' }), run('r3', 'working'));
    expect(groupBatches([a, b, c])).toEqual([]);
  });
});

describe('normalization', () => {
  test('PascalCase enums become snake_case', () => {
    const raw = {
      ...ix(),
      kind: 'PlanReview',
      status: 'ResolvedElsewhere',
      delivery: 'DeliveryUnknown',
      source: 'SelfReport',
      answer_channel: 'Keystrokes',
      action: { ...ix().action!, risk: 'Medium' },
      answer: { decision: 'AllowAlways', choices: [], text: null },
    };
    const i = normalizeInteraction(raw);
    expect([i.kind, i.status, i.delivery, i.source, i.answer_channel, i.action!.risk, i.answer!.decision]).toEqual([
      'plan_review',
      'resolved_elsewhere',
      'delivery_unknown',
      'self_report',
      'keystrokes',
      'medium',
      'allow_always',
    ]);
    expect(normalizeInteraction(i)).toEqual(i); // idempotent
  });
});

describe('answer params carry decision_rev', () => {
  test('interaction.answer always sends the revision the card showed', () => {
    const it = ix({ decision_rev: 7 });
    expect(answerParams(it, { decision: 'allow' })).toEqual({ interaction: it.id, decision: 'allow', decision_rev: 7 });
    expect(answerParams(it, { choices: { q1: ['a'] }, text: 'x' })).toEqual({ interaction: it.id, choices: { q1: ['a'] }, text: 'x', decision_rev: 7 });
    // An extra field cannot override it.
    expect(answerParams(it, { decision: 'deny', decision_rev: 99 } as never).decision_rev).toBe(7);
    expect(answerParams(ix({ decision_rev: 0 }), { decision: 'deny' }).decision_rev).toBe(0);
  });
  test('a missing revision refuses to build the call', () => {
    expect(() => answerParams(ix({ decision_rev: undefined as unknown as number }), { decision: 'allow' })).toThrow(/decision_rev/);
  });
  test('answer_batch carries decision_rev per item', () => {
    const a = item(ix({ run: 'r1', decision_rev: 3 }), run('r1', 'working'));
    const b = item(ix({ run: 'r2', decision_rev: 5 }), run('r2', 'working'));
    const [batch] = groupBatches([a, b]);
    const p = batchAnswerParams(batch!, 'deny');
    expect(p.items.map((x) => x.decision_rev).sort()).toEqual([3, 5]);
    expect(() => batchAnswerParams({ ...batch!, items: [a, item(ix({ run: 'r3', decision_rev: null as unknown as number }), run('r3', 'working'))] }, 'allow')).toThrow();
  });
});

describe('picker answers (core)', () => {
  test('expected_signature rides along with the revision; cancel is a decision', () => {
    const it = { id: 'pk', decision_rev: 2 } as never;
    expect(answerParams(it, { decision: 'cancel', expected_signature: 's1' })).toEqual({ interaction: 'pk', decision: 'cancel', expected_signature: 's1', decision_rev: 2 });
    expect(answerParams(it, { choices: { q0: ['a'] } })).toEqual({ interaction: 'pk', choices: { q0: ['a'] }, decision_rev: 2 });
  });
  test('setting a model is a mutating call', async () => {
    const { MUTATING_METHODS } = await import('../src/model');
    expect(MUTATING_METHODS.has('agent.set_model')).toBe(true);
    expect(MUTATING_METHODS.has('agent.models')).toBe(false);
  });
});
