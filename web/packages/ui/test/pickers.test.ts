import { describe, expect, test } from 'bun:test';
import { RpcError, type AgentCommand, type Interaction } from '@vibeke/core';
import {
  CommandCache,
  adjustAnswer,
  cancelAnswer,
  canCancel,
  chooseAnswer,
  commandTap,
  confirmAnswer,
  dialogOpen,
  fallbackCommands,
  filterCommands,
  initialChecked,
  isPickerChanged,
  isUnknownDialog,
  loadModels,
  normalizeCommands,
  openDialogs,
  promptInteraction,
  slashQuery,
  switchModel,
  toggleId,
} from '../src/lib/pickers';
import { interaction } from './fixtures';

const err = (kind: string, details?: unknown, code = -32000) => new RpcError('x', { code, message: kind, data: { kind, details } });

const picker = (p: Partial<Interaction> = {}): Interaction =>
  interaction({
    id: 'pk1',
    kind: 'picker',
    action: null,
    gate: false,
    title: 'Select model',
    questions: [
      {
        id: 'q0',
        prompt: 'Select model',
        header: null,
        multi: false,
        allow_free_text: false,
        options: [
          { id: 'default', label: 'Default', description: null },
          { id: 'opus', label: 'Opus', description: 'Most capable', selected: true },
          { id: 'haiku', label: 'Haiku', description: null },
        ],
      },
    ],
    picker: { name: 'model', cancel_key: 'Escape', up_down: true, source: 'screen', signature: 'sig-1' },
    ...p,
  });

describe('picker answers', () => {
  test('single-select answers with the tapped option and the card signature', () => {
    expect(chooseAnswer(picker(), 'haiku')).toEqual({ choices: { q0: ['haiku'] }, expected_signature: 'sig-1' });
  });
  test('multi-select sends the full desired set, in option order', () => {
    const it = picker();
    const opts = it.questions[0]!.options;
    let set = initialChecked(it.questions[0]!);
    expect(set).toEqual(['opus']);
    set = toggleId(opts, set, 'default');
    expect(set).toEqual(['default', 'opus']);
    set = toggleId(opts, set, 'opus');
    expect(confirmAnswer(it, set)).toEqual({ choices: { q0: ['default'] }, expected_signature: 'sig-1' });
  });
  test('adjuster and cancel', () => {
    expect(adjustAnswer(picker(), 'high')).toEqual({ choices: { adjust: ['high'] }, expected_signature: 'sig-1' });
    expect(cancelAnswer(picker())).toEqual({ decision: 'cancel', expected_signature: 'sig-1' });
  });
  test('cancel is offered only with a cancel key', () => {
    expect(canCancel(picker())).toBe(true);
    expect(canCancel(picker({ picker: { name: 'menu', cancel_key: null, source: 'screen', signature: 's' } }))).toBe(false);
    expect(canCancel(interaction())).toBe(false);
  });
  test('an unknown dialog has no options and is recognised by name', () => {
    const it = picker({ questions: [], picker: { name: 'unknown', cancel_key: 'Escape', source: 'screen', signature: 'u' } });
    expect(isUnknownDialog(it)).toBe(true);
    expect(isUnknownDialog(picker())).toBe(false);
    expect(cancelAnswer(it).decision).toBe('cancel');
  });
});

describe('composer lock and dialog_open', () => {
  test('an open picker of the run or pane locks the composer; answered ones and other kinds do not', () => {
    const items = [picker(), picker({ id: 'old', status: 'answered' }), interaction({ id: 'ap' }), picker({ id: 'other', run: 'r9', pane: 'p9' })];
    expect(openDialogs(items, 'r1', 'p1').map((i) => i.id)).toEqual(['pk1']);
    expect(openDialogs(items, null, 'p9').map((i) => i.id)).toEqual(['other']);
    expect(openDialogs([], 'r1', 'p1')).toEqual([]);
  });
  test('dialog_open conflict carries the interaction id (plain or embedded object)', () => {
    expect(dialogOpen(err('conflict', { reason: 'dialog_open', interaction: 'pk1' }))).toEqual({ interaction: 'pk1' });
    expect(dialogOpen(err('conflict', { reason: 'dialog_open', interaction: { id: 'pk2' } }))).toEqual({ interaction: 'pk2' });
    expect(dialogOpen(err('conflict', { reason: 'dialog_open' }))).toEqual({ interaction: null });
  });
  test('other errors are not dialog_open', () => {
    expect(dialogOpen(err('conflict', { reason: 'busy' }))).toBeNull();
    expect(dialogOpen(err('stale', { reason: 'dialog_open' }))).toBeNull();
    expect(dialogOpen(new Error('x'))).toBeNull();
  });
  test('picker_changed is its own conflict', () => {
    expect(isPickerChanged(err('conflict', { reason: 'picker_changed' }))).toBe(true);
    expect(isPickerChanged(err('conflict', { reason: 'dialog_open' }))).toBe(false);
    expect(isPickerChanged(err('stale'))).toBe(false);
  });
  test('a prompt result without a turn is fine; an interaction id is picked up', () => {
    expect(promptInteraction({ turn_started: false })).toBeNull();
    expect(promptInteraction({})).toBeNull();
    expect(promptInteraction(undefined)).toBeNull();
    expect(promptInteraction({ turn_started: false, interaction: 'pk1' })).toBe('pk1');
  });
});

describe('answering a picker through the app model', () => {
  async function model(fail: unknown) {
    const { AppModel } = await import('../src/app/model');
    const kv = new Map<string, string>();
    const platform = {
      kv: { get: (k: string) => kv.get(k) ?? null, set: (k: string, v: string) => void kv.set(k, v), remove: (k: string) => void kv.delete(k) },
      clock: { now: () => 0, setTimeout: () => 0, clearTimeout: () => {} },
      defaultDeviceName: 'test',
    };
    const app = new AppModel(platform as never);
    const calls: [string, Record<string, unknown>][] = [];
    let refreshed = 0;
    const conn = {
      request: async (m: string, p: Record<string, unknown>) => {
        calls.push([m, p]);
        if (fail) throw fail;
        return { interaction: {}, delivery: { channel: 'keystrokes' } };
      },
      refresh: async () => void refreshed++,
    };
    (app as unknown as { conn: () => unknown }).conn = () => conn;
    return { app, calls, refreshed: () => refreshed };
  }

  test('the signature travels with the answer', async () => {
    const { app, calls } = await model(null);
    await app.answer('h1', picker({ decision_rev: 3 }), chooseAnswer(picker(), 'haiku'), 'haiku');
    expect(calls[0]).toEqual(['interaction.answer', { interaction: 'pk1', choices: { q0: ['haiku'] }, expected_signature: 'sig-1', decision_rev: 3 }]);
    await app.answer('h1', picker({ id: 'pk2' }), cancelAnswer(picker()), 'cancel');
    expect(calls[1]![1]).toMatchObject({ decision: 'cancel', expected_signature: 'sig-1' });
  });

  test('picker_changed refreshes and clears the card state instead of showing an error', async () => {
    const { app, refreshed } = await model(err('conflict', { reason: 'picker_changed' }));
    await app.answer('h1', picker(), chooseAnswer(picker(), 'haiku'), 'haiku');
    expect(app.answers.get('h1/pk1')).toBeUndefined();
    expect(refreshed()).toBe(1);
    expect(app.toasts.get().some((x) => x.tone === 'error')).toBe(false);
    expect(app.toasts.get()[0]?.tone).toBe('info');
  });
});

describe('slash commands', () => {
  const cmds: AgentCommand[] = [
    { name: '/model', description: 'Switch the model', takes_arg: true, opens_picker: true, dangerous: false },
    { name: '/compact', description: 'Summarize the conversation', takes_arg: true, opens_picker: false, dangerous: false },
    { name: '/clear', description: 'Start fresh', takes_arg: false, opens_picker: false, dangerous: true },
    { name: '/memory', description: 'Edit memory files', takes_arg: false, opens_picker: false, dangerous: false },
  ];
  test('only a bare slash word is a query', () => {
    expect(slashQuery('/')).toBe('');
    expect(slashQuery('/Mo')).toBe('mo');
    expect(slashQuery('/model gpt')).toBeNull();
    expect(slashQuery('hello /model')).toBeNull();
    expect(slashQuery('/path/to/file')).toBeNull();
  });
  test('filters by prefix first, then by substring in name or description', () => {
    expect(filterCommands(cmds, '/').map((c) => c.name)).toHaveLength(4);
    expect(filterCommands(cmds, '/m').map((c) => c.name)).toEqual(['/model', '/memory', '/compact']);
    expect(filterCommands(cmds, '/ear').map((c) => c.name)).toEqual(['/clear']);
    expect(filterCommands(cmds, '/summar').map((c) => c.name)).toEqual(['/compact']);
    expect(filterCommands(cmds, '/zzz')).toEqual([]);
    expect(filterCommands(cmds, 'no slash')).toEqual([]);
  });
  test('tapping: arguments are completed, dangerous commands need a second tap, picker commands just send', () => {
    expect(commandTap(cmds[1]!, null)).toEqual({ do: 'insert', text: '/compact ' });
    expect(commandTap(cmds[0]!, null)).toEqual({ do: 'send', text: '/model' });
    expect(commandTap(cmds[2]!, null)).toEqual({ do: 'arm' });
    expect(commandTap(cmds[2]!, '/other')).toEqual({ do: 'arm' });
    expect(commandTap(cmds[2]!, '/clear')).toEqual({ do: 'send', text: '/clear' });
    expect(commandTap(cmds[3]!, null)).toEqual({ do: 'send', text: '/memory' });
  });
  test('host lists are normalised', () => {
    expect(normalizeCommands({ commands: [{ name: 'model', description: 'x' }, { name: '/model' }, { name: '' }, null] }).map((c) => c.name)).toEqual(['/model']);
    expect(normalizeCommands(undefined)).toEqual([]);
  });
  test('cache loads once per run and fails soft to the built-in palette', async () => {
    const cache = new CommandCache();
    let n = 0;
    const ok = { request: async () => (n++, { commands: [{ name: '/x', description: '', takes_arg: false, opens_picker: false, dangerous: false }], source: 'protocol' }) };
    const a = await cache.load('h/r1', ok as never, 'r1', 'claude');
    await cache.load('h/r1', ok as never, 'r1', 'claude');
    expect(n).toBe(1);
    expect(a.map((c) => c.name)).toEqual(['/x']);
    expect(cache.peek('h/r1')).toEqual(a);

    const old = { request: async () => { throw err('unsupported', undefined, -32601); } };
    const b = await cache.load('h/r2', old as never, 'r2', 'claude');
    expect(b).toEqual(fallbackCommands('claude'));
    expect(b.length).toBeGreaterThan(0);
    // No connection: fallback now, but a later load may try again.
    const c = await cache.load('h/r3', undefined, 'r3', 'codex');
    expect(c).toEqual(fallbackCommands('codex'));
    let m = 0;
    await cache.load('h/r3', { request: async () => (m++, { commands: [] }) } as never, 'r3', 'codex');
    expect(m).toBe(1);
  });
});

describe('model switcher', () => {
  const models = { models: [{ id: 'a', label: 'A', current: true }, { id: 'b', label: 'B', current: false }], source: 'protocol' };
  test('lists models when the host can', async () => {
    const r = await loadModels({ request: async () => models } as never, 'r1');
    expect(r).toEqual({ kind: 'models', models: models.models });
  });
  test('unsupported, a missing method or an empty list fall back to /model', async () => {
    expect(await loadModels({ request: async () => { throw err('unsupported'); } } as never, 'r1')).toEqual({ kind: 'fallback' });
    expect(await loadModels({ request: async () => { throw err('method_not_found', undefined, -32601); } } as never, 'r1')).toEqual({ kind: 'fallback' });
    expect(await loadModels({ request: async () => ({ models: [], source: 'screen' }) } as never, 'r1')).toEqual({ kind: 'fallback' });
  });
  test('other failures are shown, not swallowed', async () => {
    const r = await loadModels({ request: async () => { throw err('internal'); } } as never, 'r1');
    expect(r.kind).toBe('error');
  });
  test('set_model for the session; unsupported sends /model so the native picker appears', async () => {
    const calls: unknown[] = [];
    const sent: string[] = [];
    const send = async (t: string) => (sent.push(t), true);
    expect(await switchModel({ request: async (m: string, p: unknown) => (calls.push([m, p]), { run: {} }) } as never, send, 'r1', 'b')).toBe('set');
    expect(calls).toEqual([['agent.set_model', { target: 'r1', model: 'b', scope: 'session' }]]);
    expect(sent).toEqual([]);
    expect(await switchModel({ request: async () => { throw err('unsupported'); } } as never, send, 'r1', 'b')).toBe('picker');
    expect(sent).toEqual(['/model']);
    await expect(switchModel({ request: async () => { throw err('forbidden'); } } as never, send, 'r1', 'b')).rejects.toBeInstanceOf(RpcError);
  });
});
