import { describe, expect, test } from 'bun:test';
import type { BgTask, TranscriptTurn } from '@vibeke/core';
import { bgCount, bgSummary, bgTasksOf, bgToolIds } from '../src/lib/bg-tasks';
import { turnBlocks, turnHasWork, turnStats, turnSteps, turnToolIds } from '../src/lib/conversation';
import { isRunEvent } from '../src/lib/live-transcript';
import { toolSummary } from '../src/lib/tool-summary';
import { run } from './fixtures';

const task = (id: string, since_ms: number, extra: Partial<BgTask> = {}): BgTask => ({ id, kind: 'shell', label: `sleep ${id}`, since_ms, source: 'structured', ...extra });
const exec = (value: string) => ({ value: value as 'idle', since_ms: 0, source: 'structured' as const, confidence: 1, detail: null });

describe('background tasks of a run', () => {
  test('older servers send none', () => {
    expect(bgTasksOf(run())).toEqual([]);
    expect(bgCount(run())).toBe(0);
    expect(bgSummary(run())).toBeNull();
  });

  test('an idle run with tasks shows the count, oldest task first', () => {
    const r = run({ bg_tasks: [task('b', 20), task('a', 10)] });
    expect(bgTasksOf(r).map((b) => b.id)).toEqual(['a', 'b']);
    expect(bgCount(r)).toBe(2);
    expect(bgSummary(r)).toBe('2 in background');
  });

  test('a working, failed or exited run shows no background chip', () => {
    for (const v of ['working', 'starting', 'error', 'exited']) expect(bgCount(run({ execution: exec(v), bg_tasks: [task('a', 1)] }))).toBe(0);
  });

  test('tool call ids of the tasks', () => {
    const r = run({ bg_tasks: [task('a', 1, { tool_use_id: 'c1' }), task('b', 2), task('c', 3, { tool_use_id: null })] });
    expect([...bgToolIds(r)]).toEqual(['c1']);
  });
});

describe('bg_event items', () => {
  const ev = { kind: 'bg_event', id: 't1', tool_use_id: 'c1', status: 'completed', summary: ' Background command "sleep 5"\ncompleted ', ts: 5 };
  const turn: TranscriptTurn = { n: 3, items: [ev, { kind: 'tool_call', tool: 'Bash', summary: '{"command":"ls"}', id: 'c2' }, { kind: 'text', role: 'assistant', text: 'Done.' }] };

  test('become a system row, not a user bubble', () => {
    const blocks = turnSteps(turn);
    expect(blocks[0]).toEqual({ k: 'bg_event', key: '3:0', id: 't1', toolUseId: 'c1', status: 'completed', summary: 'Background command "sleep 5" completed' });
    expect(blocks.some((b) => b.k === 'user')).toBe(false);
  });

  test('an unknown status reads as a plain event', () => {
    const b = turnSteps({ n: 1, items: [{ kind: 'bg_event', id: 'x', status: 'weird', summary: 's' }] })[0];
    expect(b).toMatchObject({ k: 'bg_event', status: 'event', toolUseId: null });
  });

  test('do not count as tools and do not fold with steps', () => {
    expect(turnStats(turn).tools).toBe(1);
    const blocks = turnBlocks(turn);
    expect(blocks.map((b) => b.k)).toEqual(['bg_event', 'tool', 'text']);
  });

  test('a turn of only a bg_event has no footer-worthy work', () => {
    expect(turnHasWork({ n: 1, items: [ev] })).toBe(false);
    expect(turnHasWork(turn)).toBe(true);
  });

  test('tool ids of a turn', () => {
    expect([...turnToolIds(turn)]).toEqual(['c2']);
  });
});

describe('related lib changes', () => {
  test('bg_tasks_changed refetches the transcript', () => {
    expect(isRunEvent({ type: 'agent.bg_tasks_changed', subject: { run: 'r1' } } as never, 'r1', null)).toBe(true);
  });

  test('labels for the task tools', () => {
    expect(toolSummary('TaskOutput', '{"task_id":"b1"}')).toEqual({ kind: 'shell', label: 'Shell output', detail: 'b1' });
    expect(toolSummary('TaskStop', '{"task_id":"b1"}').label).toBe('Stop task');
    expect(toolSummary('KillShell', '{"shell_id":"b2"}')).toEqual({ kind: 'shell', label: 'Stop task', detail: 'b2' });
    expect(toolSummary('Monitor', '{"description":"watch logs","command":"tail -f x"}')).toEqual({ kind: 'shell', label: 'Monitor', detail: 'watch logs' });
    expect(toolSummary('write_stdin', '{"session_id":"7","chars":"q\\n"}').label).toBe('Shell input');
  });
});
