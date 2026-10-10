import { describe, expect, test } from 'bun:test';
import type { TranscriptItem, TranscriptTurn } from '@vibeke/core';
import { cleanUserText, foldSteps, turnBlocks, turnHasWork, turnStats, turnSteps, turnText, workedFor, FOLD_KEEP } from '../src/lib/conversation';
import { jsonArrayField, jsonField, patchPaths, shellCommand, toolSummary } from '../src/lib/tool-summary';

describe('tool summaries', () => {
  test('Claude tools: command, paths, pattern, url, description', () => {
    expect(toolSummary('Bash', '{"command":"git status --short","description":"Show status"}')).toEqual({ kind: 'shell', label: 'Shell', detail: 'git status --short' });
    expect(toolSummary('Read', '{"file_path":"/repo/src/app.ts","limit":40}', '/repo')).toEqual({ kind: 'read', label: 'Read', detail: 'src/app.ts' });
    expect(toolSummary('Edit', '{"file_path":"/repo/src/a.ts","old_string":"x","new_string":"y"}', '/repo').detail).toBe('src/a.ts');
    expect(toolSummary('Write', '{"file_path":"/elsewhere/b.md","content":"# hi"}', '/repo')).toEqual({ kind: 'write', label: 'Write', detail: '/elsewhere/b.md' });
    expect(toolSummary('Grep', '{"pattern":"useGitStatus","path":"/repo/web","output_mode":"files_with_matches"}', '/repo').detail).toBe('useGitStatus in web');
    expect(toolSummary('WebFetch', '{"url":"https://example.com/docs","prompt":"summarize"}')).toEqual({ kind: 'web', label: 'Fetch', detail: 'https://example.com/docs' });
    expect(toolSummary('Task', '{"description":"Explore the router","prompt":"Find…","subagent_type":"Explore"}')).toEqual({ kind: 'task', label: 'Agent', detail: 'Explore the router' });
  });

  test('a command cut at 160 characters still reads (no closing quote)', () => {
    const cut = '{"command":"for p in highlight relay protocol client; do echo \\"$p\\"; grep -rn something-very-long-here src/ | head -20; done && echo finished with a very long tail that is cu';
    const s = toolSummary('Bash', cut);
    expect(s.label).toBe('Shell');
    expect(s.detail.startsWith('for p in highlight relay protocol client; do echo "$p";')).toBe(true);
    expect(s.detail.endsWith('tail that is cu')).toBe(true);
  });

  test('Codex shell (argv, bash -lc), exec_command and apply_patch', () => {
    expect(toolSummary('shell', '{"command":["bash","-lc","sed -n \'1,80p\' docs/release.md"],"workdir":"/repo"}')).toEqual({ kind: 'shell', label: 'Shell', detail: "sed -n '1,80p' docs/release.md" });
    expect(toolSummary('shell', '{"command":["rg","-n","TODO","src"]}').detail).toBe('rg -n TODO src');
    expect(toolSummary('exec_command', '{"cmd":"cargo test -p vk-server"}').detail).toBe('cargo test -p vk-server');
    const patch = '{"input":"*** Begin Patch\\n*** Update File: src/a.ts\\n@@\\n-x\\n+y\\n*** Add File: src/b.ts\\n+z\\n*** End Patch"}';
    expect(toolSummary('apply_patch', patch)).toEqual({ kind: 'edit', label: 'Edit', detail: 'src/a.ts, src/b.ts' });
    expect(toolSummary('shell', '{"command":["apply_patch","*** Begin Patch\\n*** Delete File: old.rs\\n*** End Patch"]}')).toEqual({ kind: 'edit', label: 'Edit', detail: 'old.rs' });
    // local_shell_call: no tool name, the action object.
    expect(toolSummary(null, '{"type":"exec","command":["zsh","-c","ls -la"]}').detail).toBe('ls -la');
  });

  test('unknown and MCP tools fall back to the first string argument', () => {
    expect(toolSummary('mcp__github__create_issue', '{"title":"Fix it","body":"…"}')).toEqual({ kind: 'other', label: 'Create issue', detail: 'Fix it' });
    expect(toolSummary('Mystery', 'null').detail).toBe('');
  });

  test('field helpers', () => {
    expect(jsonField('{"a":"x\\"y","b":"z"}', 'a')).toBe('x"y');
    expect(jsonField('{"a":"dangling\\', 'a')).toBe('dangling');
    expect(jsonArrayField('{"command":["a","b c"', 'command')).toEqual(['a', 'b c']);
    expect(shellCommand(['/bin/bash', '-lc', 'echo hi'])).toBe('echo hi');
    expect(patchPaths('*** Update File: a.ts\n*** Update File: a.ts')).toEqual(['a.ts']);
  });
});

const call = (i: number, tool = 'Bash'): TranscriptItem => ({ kind: 'tool_call', tool, summary: `{"command":"step ${i}"}`, id: `c${i}`, ts: 1000 + i });
const result = (i: number, error = false): TranscriptItem => ({ kind: 'tool_result', summary: `out ${i}`, id: `c${i}`, error, ts: 1000 + i });

describe('conversation grouping', () => {
  test('pairs calls with results by id; user and assistant text become blocks', () => {
    const turn: TranscriptTurn = {
      n: 7,
      items: [
        { kind: 'text', role: 'user', text: 'Fix the build' },
        call(1),
        { kind: 'thinking', text: 'checking' },
        result(1, true),
        { kind: 'text', role: 'assistant', text: 'Done.' },
      ],
    };
    const b = turnSteps(turn);
    expect(b.map((x) => x.k)).toEqual(['user', 'tool', 'thinking', 'text']);
    const tool = b[1] as Extract<(typeof b)[number], { k: 'tool' }>;
    expect(tool.result?.error).toBe(true);
    expect(tool.key).toBe('7:1');
  });

  test('more than five consecutive steps fold into "N steps", keeping the latest visible', () => {
    const items: TranscriptItem[] = [{ kind: 'text', role: 'user', text: 'go' }];
    for (let i = 1; i <= 8; i++) items.push(call(i), result(i));
    items.push({ kind: 'text', role: 'assistant', text: 'ok' });
    const blocks = turnBlocks({ n: 1, items });
    expect(blocks.map((x) => x.k)).toEqual(['user', 'steps', 'tool', 'tool', 'tool', 'text']);
    const folded = blocks[1] as Extract<(typeof blocks)[number], { k: 'steps' }>;
    expect(folded.steps).toHaveLength(8 - FOLD_KEEP);
  });

  test('five steps or fewer stay as they are; text breaks a run', () => {
    const five = Array.from({ length: 5 }, (_, i) => ({ k: 'thinking' as const, key: `${i}`, text: 'x' }));
    expect(foldSteps(five)).toEqual(five);
    const split = [...five, { k: 'text' as const, key: 't', text: 'mid' }, ...five];
    expect(foldSteps(split).some((b) => b.k === 'steps')).toBe(false);
  });

  test('user text: harness wrappers collapse to what was typed', () => {
    expect(cleanUserText('<command-name>/release-beta</command-name><command-args></command-args>')).toBe('/release-beta');
    expect(cleanUserText('<command-message>x</command-message><command-name>/review</command-name><command-args>42</command-args>')).toBe('/review 42');
    expect(cleanUserText('hello<system-reminder>secret</system-reminder>')).toBe('hello');
    expect(cleanUserText('<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n</task-notification>')).toBe('');
    expect(cleanUserText('<local-command-stdout></local-command-stdout>')).toBe('');
    expect(cleanUserText('<local-command-caveat>Caveat: the messages below were generated by the user while running local commands.</local-command-caveat>')).toBe('');
  });
});

describe('turn footer', () => {
  test('worked-for formatting', () => {
    expect(workedFor(0)).toBe('0s');
    expect(workedFor(45_400)).toBe('45s');
    expect(workedFor(78_000)).toBe('1m 18s');
    expect(workedFor((31 * 60 + 46) * 1000)).toBe('31m 46s');
    expect(workedFor((2 * 3600 + 5 * 60 + 9) * 1000)).toBe('2h 5m');
  });

  test('stats prefer the server fields, else derive them from the items', () => {
    const items = [{ kind: 'text', role: 'user', text: 'go', ts: 1_000 }, call(1, 'Task'), call(2), { kind: 'text', role: 'assistant', text: 'A', ts: 4_500 }] as TranscriptItem[];
    expect(turnStats({ n: 1, items, duration_ms: 9_250, tool_count: 3, subagent_count: 2 })).toEqual({ durationMs: 9_250, tools: 3, subagents: 2 });
    expect(turnStats({ n: 1, items })).toEqual({ durationMs: 3_500, tools: 2, subagents: 1 });
    expect(turnStats({ n: 1, items: [{ kind: 'text', role: 'user', text: 'x' }] })).toEqual({ durationMs: null, tools: 0, subagents: 0 });
  });

  test('copy text and whether a footer shows', () => {
    const t: TranscriptTurn = { n: 1, items: [{ kind: 'text', role: 'user', text: 'q' }, { kind: 'text', role: 'assistant', text: ' one ' }, call(1), { kind: 'text', role: 'assistant', text: 'two' }] };
    expect(turnText(t)).toBe('one\n\ntwo');
    expect(turnHasWork(t)).toBe(true);
    expect(turnHasWork({ n: 2, items: [{ kind: 'text', role: 'user', text: 'q' }] })).toBe(false);
  });
});

describe('image items', () => {
  test('a usable image is a block in place; unknown or unusable ones are dropped', () => {
    const turn: TranscriptTurn = {
      n: 4,
      items: [
        { kind: 'text', role: 'user', text: 'look' },
        { kind: 'image', mime: 'image/png', ref: '0:0', size: 1200 },
        { kind: 'image', mime: 'image/svg+xml', ref: '1:0' },
        { kind: 'image', mime: 'image/png' },
        { kind: 'hologram', mime: 'image/png', ref: '2:0' },
        { kind: 'text', role: 'assistant', text: 'seen' },
      ],
    };
    const blocks = turnSteps(turn);
    expect(blocks.map((b) => b.k)).toEqual(['user', 'image', 'text']);
    expect(blocks[1]).toEqual({ k: 'image', key: '4:1', mime: 'image/png', ref: '0:0', size: 1200 });
  });
});
