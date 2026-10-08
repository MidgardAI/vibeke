import { describe, expect, test } from 'bun:test';
import { parseAnsi, stripAnsi } from '../src/lib/ansi';
import { composerShowsStop, destructiveReason, isNoEchoPrompt } from '../src/lib/guards';
import { NO_MODS, chord, cycleMod, isValidKey, keyLabel, press, queueAdd, queueRemoveAt } from '../src/lib/keys';
import { MAX_DEPTH, parseInline, parseMarkdown, safeHref, type Block } from '../src/lib/markdown';
import { parseDiff, tokenize, langOf } from '../src/lib/highlight';
import { nextPollDelay } from '../src/lib/poll';
import { base64Std, shortDuration, shortPath } from '../src/lib/format';
import { parsePrefs, PrefsStore, DEFAULT_PREFS } from '../src/lib/prefs';
import { quickRepliesFor, slashCommandsFor } from '../src/lib/harness';

describe('ansi', () => {
  test('SGR colours become styles, other escapes vanish', () => {
    const lines = parseAnsi('\x1b[1;31mERR\x1b[0m ok\x1b]0;title\x07\x1b[2J\nnext\x1b[38;5;82mG\x1b[38;2;1;2;3mT');
    expect(lines.length).toBe(2);
    expect(lines[0]![0]).toEqual({ text: 'ERR', style: { bold: true, fg: 'var(--ansi-red)' } });
    expect(lines[0]![1]!.text).toBe(' ok');
    expect(lines[1]!.map((s) => s.text).join('')).toBe('nextGT');
    expect(lines[1]![2]!.style.fg).toBe('rgb(1,2,3)');
    expect(stripAnsi('a\x1b[0;32mb\x1bPq#0\x1b\\c')).toBe('abc');
  });
  test('control characters are dropped, tabs expanded', () => {
    expect(stripAnsi('a\x07b\tc\r')).toBe('ab    c');
  });
});

describe('destructive guard', () => {
  const bad = ['rm -rf build', 'sudo rm x', 'git push --force origin main', 'git push -f', 'git reset --hard HEAD~1', 'DROP TABLE users;', 'drop database prod', 'git clean -fdx', 'mkfs.ext4 /dev/sda1', 'dd if=/dev/zero of=/dev/sda', 'truncate table x'];
  const good = ['rm file.txt', 'git push origin main', 'assume the forced reset', 'dropdown table', 'git reset HEAD file', 'ls -r', 'please run the tests'];
  for (const s of bad) test(`flags: ${s}`, () => expect(destructiveReason(s)).not.toBeNull());
  for (const s of good) test(`allows: ${s}`, () => expect(destructiveReason(s)).toBeNull());
  test('first matching reason wins', () => expect(destructiveReason('sudo rm -rf /')).toBe('rm -r (recursive delete)'));
});

describe('composer send / stop', () => {
  const run = (value: string) => ({ id: 'r1', execution: { value } });
  test('Stop only while working with nothing waiting on the user', () => {
    expect(composerShowsStop(run('working'), [], '')).toBe(true);
    expect(composerShowsStop(run('working'), null, '  ')).toBe(true);
    // Text in the box is a message to send.
    expect(composerShowsStop(run('working'), [], 'hi')).toBe(false);
    for (const v of ['idle', 'exited', 'unknown', 'error', 'rate_limited', 'starting']) expect(composerShowsStop(run(v), [], '')).toBe(false);
    expect(composerShowsStop(null, [], '')).toBe(false);
  });
  test('an open interaction on the run (waiting for approval) shows the send button', () => {
    expect(composerShowsStop(run('working'), [{ run: 'r1', status: 'open' }], '')).toBe(false);
    // Answered ones, or open ones on other runs, do not count.
    expect(composerShowsStop(run('working'), [{ run: 'r1', status: 'answered' }, { run: 'r2', status: 'open' }], '')).toBe(true);
  });
});

describe('no-echo detection', () => {
  test('password prompts on the last non-empty line', () => {
    expect(isNoEchoPrompt('$ sudo ls\n[sudo] password for alice: \n\n')).toBe(true);
    expect(isNoEchoPrompt('Enter passphrase for key /k:')).toBe(true);
    expect(isNoEchoPrompt('Password:')).toBe(true);
    expect(isNoEchoPrompt('password: hunter2 accepted\n$ ')).toBe(false);
    expect(isNoEchoPrompt('reset your password at example.com')).toBe(false);
  });
});

describe('keys', () => {
  test('sticky modifiers cycle off → once → locked → off', () => {
    expect(cycleMod('off')).toBe('once');
    expect(cycleMod('once')).toBe('locked');
    expect(cycleMod('locked')).toBe('off');
  });
  test('once is consumed by a press, locked stays', () => {
    const r = press('c', { ...NO_MODS, ctrl: 'once', shift: 'locked' });
    expect(r.key).toBe('ctrl+shift+c');
    expect(r.mods).toEqual({ ctrl: 'off', alt: 'off', shift: 'locked' });
    expect(press('up', NO_MODS).key).toBe('up');
  });
  test('chord grammar matches the server key grammar', () => {
    expect(chord('+', { ...NO_MODS, ctrl: 'once' })).toBe('ctrl++');
    expect(chord('tab', { ...NO_MODS, shift: 'once', alt: 'once' })).toBe('alt+shift+tab');
    for (const k of ['ctrl+c', 'shift+tab', 'esc', 'enter', 'f12', 'ctrl++', 'a', 'alt+x', 'pagedown']) expect(isValidKey(k)).toBe(true);
    for (const k of ['C-c', 'cmd+x', 'escapee', '', 'f25']) expect(isValidKey(k)).toBe(false);
  });
  test('chord queue add/remove', () => {
    let q = queueAdd({ keys: [] }, 'esc');
    q = queueAdd(q, 'ctrl+c');
    q = queueAdd(q, 'enter');
    expect(queueRemoveAt(q, 1).keys).toEqual(['esc', 'enter']);
  });
  test('labels', () => {
    expect(keyLabel('ctrl+c')).toBe('⌃C');
    expect(keyLabel('enter')).toBe('⏎');
    expect(keyLabel('shift+tab')).toBe('⇧⇥');
  });
});

describe('markdown', () => {
  test('adversarial nesting does not overflow the stack; deep content stays literal', () => {
    const t0 = Date.now();
    const b = parseMarkdown('> '.repeat(40000) + 'x');
    let depth = 0;
    let node: Block | undefined = b[0];
    while (node?.t === 'quote') {
      depth++;
      node = node.c[0];
    }
    expect(depth).toBeLessThanOrEqual(MAX_DEPTH);
    expect(node?.t).toBe('raw');
    expect((node as { v: string }).v.endsWith('x')).toBe(true);
    // Inline and list nesting too.
    expect(() => parseInline('*'.repeat(100000) + 'x' + '*'.repeat(100000))).not.toThrow();
    expect(() => parseMarkdown('**'.repeat(50000) + 'x')).not.toThrow();
    const nestedList = Array.from({ length: 5000 }, (_, i) => `${' '.repeat(i * 2)}- item`).join('\n');
    expect(() => parseMarkdown(nestedList)).not.toThrow();
    expect(() => parseMarkdown('_a '.repeat(200000))).not.toThrow();
    expect(Date.now() - t0).toBeLessThan(5000);
  });
  test('raw HTML stays text, links only http(s)', () => {
    const b = parseMarkdown('<script>alert(1)</script> [x](javascript:alert(1)) [y](https://a.b/c)');
    expect(b[0]!.t).toBe('p');
    const p = b[0] as { t: 'p'; c: any[] };
    expect(p.c[0].t).toBe('text');
    expect(p.c[0].v.startsWith('<script>alert(1)</script> x')).toBe(true);
    expect(p.c.filter((n: any) => n.t === 'link').length).toBe(1);
    expect(p.c[1]).toMatchObject({ t: 'link', href: 'https://a.b/c' });
    expect(safeHref('data:text/html,hi')).toBeNull();
  });
  test('blocks: headings, lists, tasks, code, quote', () => {
    const b = parseMarkdown('# Plan\n\n1. one\n2. two\n\n- [x] done\n- [ ] todo\n\n```ts\nconst a = 1;\n```\n\n> note');
    expect(b.map((x) => x.t)).toEqual(['h', 'list', 'list', 'code', 'quote']);
    expect((b[1] as any).ordered).toBe(true);
    expect((b[2] as any).items.map((i: any) => i.checked)).toEqual([true, false]);
    expect((b[3] as any).v).toBe('const a = 1;');
  });
  test('inline emphasis and snake_case', () => {
    expect(parseInline('**bold** and *em* and `code` snake_case_name')).toEqual([
      { t: 'strong', c: [{ t: 'text', v: 'bold' }] },
      { t: 'text', v: ' and ' },
      { t: 'em', c: [{ t: 'text', v: 'em' }] },
      { t: 'text', v: ' and ' },
      { t: 'code', v: 'code' },
      { t: 'text', v: ' snake_case_name' },
    ]);
  });
});

describe('diff + highlight', () => {
  test('parses hunks with line numbers', () => {
    const d = parseDiff('diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1,2 +1,3 @@\n a\n-b\n+c\n+d\n');
    expect(d.filter((l) => l.kind === 'add').map((l) => [l.text, l.newNo])).toEqual([['c', 2], ['d', 3]]);
    expect(d.find((l) => l.kind === 'del')).toMatchObject({ text: 'b', oldNo: 2 });
    expect(d.find((l) => l.kind === 'ctx')).toMatchObject({ text: 'a', oldNo: 1, newNo: 1 });
  });
  test('tokenizes keywords, strings, comments', () => {
    const tk = tokenize('const x = "hi"; // c', 'ts');
    expect(tk.find((t) => t.k === 'kw')?.v).toBe('const');
    expect(tk.find((t) => t.k === 'str')?.v).toBe('"hi"');
    expect(tk[tk.length - 1]).toEqual({ k: 'com', v: '// c' });
    expect(tokenize('# hash', 'py')[0]!.k).toBe('com');
    expect(langOf('src/a.test.TS')).toBe('ts');
  });
});

describe('poll cadence', () => {
  const base = { visible: true, locked: false, online: true, working: false, burst: 0 };
  test('working 1.5 s, idle 4 s, burst 300 ms, paused when hidden/locked/offline', () => {
    expect(nextPollDelay(base)).toBe(4000);
    expect(nextPollDelay({ ...base, working: true })).toBe(1500);
    expect(nextPollDelay({ ...base, burst: 2 })).toBe(300);
    expect(nextPollDelay({ ...base, visible: false })).toBeNull();
    expect(nextPollDelay({ ...base, locked: true })).toBeNull();
    expect(nextPollDelay({ ...base, online: false })).toBeNull();
  });
});

describe('format', () => {
  test('standard base64 with padding for the server decoder', () => {
    const bytes = new Uint8Array([251, 255, 0, 1]);
    expect(base64Std(bytes)).toBe(Buffer.from(bytes).toString('base64'));
    const big = new Uint8Array(100_000).map((_, i) => i * 7);
    expect(base64Std(big)).toBe(Buffer.from(big).toString('base64'));
  });
  test('durations and paths', () => {
    expect(shortDuration(2000)).toBe('now');
    expect(shortDuration(90_000)).toBe('1m');
    expect(shortDuration(3 * 3600_000)).toBe('3h');
    expect(shortPath('/Users/alice/code/vibeke/web')).toBe('…/vibeke/web');
    expect(shortPath('/Users/alice/x')).toBe('~/x');
  });
});

describe('prefs', () => {
  test('lenient parse with defaults', () => {
    expect(parsePrefs(null)).toEqual(DEFAULT_PREFS);
    expect(parsePrefs('not json')).toEqual(DEFAULT_PREFS);
    const p = parsePrefs(JSON.stringify({ theme: 'light', termFont: 99, panelWidth: 5000, collapsed: ['done', 1], hostFilter: 7, pins: ['a/b', 3], quickReplies: { claude: ['go', '', 5] }, bogus: 1 }));
    expect(p.theme).toBe('light');
    expect(p.panelWidth).toBe(760);
    expect(p.collapsed).toEqual(['done']);
    expect(p.hostFilter).toBeNull();
    expect(DEFAULT_PREFS.theme).toBe('dark');
    expect(p.termFont).toBe(DEFAULT_PREFS.termFont);
    expect(p.pins).toEqual(['a/b']);
    expect(p.quickReplies).toEqual({ claude: ['go'] });
    expect((p as any).bogus).toBeUndefined();
  });
  test('store persists, pins toggle, seen marks', () => {
    const m = new Map<string, string>();
    const kv = { get: (k: string) => m.get(k) ?? null, set: (k: string, v: string) => void m.set(k, v), remove: (k: string) => void m.delete(k) };
    const s = new PrefsStore(kv);
    s.togglePin('h/p');
    s.markSeen('h/r', 3);
    expect(new PrefsStore(kv).get().pins).toEqual(['h/p']);
    s.togglePin('h/p');
    expect(s.get().pins).toEqual([]);
    expect(s.get().seenDone).toEqual({ 'h/r': 3 });
  });
});

describe('harness data', () => {
  test('quick replies fall back per harness then default', () => {
    expect(quickRepliesFor('claude', {})).toContain('run the tests');
    expect(quickRepliesFor('codex', { '*': ['x'] })).toEqual(['x']);
    expect(quickRepliesFor('codex', { codex: ['y'], '*': ['x'] })).toEqual(['y']);
    expect(slashCommandsFor('claude').map((c) => c.command)).toContain('/compact');
    expect(slashCommandsFor('codex').map((c) => c.command)).toContain('/diff');
    expect(slashCommandsFor(null)).toEqual([]);
  });
});
