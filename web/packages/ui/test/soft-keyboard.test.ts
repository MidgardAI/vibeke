import { describe, expect, test } from 'bun:test';
import { keyboardOpen } from '../src/lib/soft-keyboard';
import { plainOutput } from '../src/lib/copy-output';
import { requestFileLine, takeFileLine } from '../src/lib/file-focus';
import { parseMarkdown } from '../src/lib/markdown';

const base = { innerHeight: 800, vvHeight: 800, editableFocused: true, coarse: true };

describe('keyboardOpen', () => {
  test('a much shorter visual viewport with a text field focused', () => {
    expect(keyboardOpen({ ...base, vvHeight: 480 })).toBe(true);
  });
  test('not without a finger, a focused field or a real height change', () => {
    expect(keyboardOpen({ ...base, vvHeight: 480, coarse: false })).toBe(false);
    expect(keyboardOpen({ ...base, vvHeight: 480, editableFocused: false })).toBe(false);
    expect(keyboardOpen({ ...base, vvHeight: 760 })).toBe(false);
  });
  test('pinch zoom is not a keyboard', () => {
    expect(keyboardOpen({ ...base, vvHeight: 400, vvScale: 2 })).toBe(false);
  });
  test('browsers that shrink the window use the tallest height seen', () => {
    expect(keyboardOpen({ ...base, innerHeight: 500, vvHeight: 500, baseHeight: 800 })).toBe(true);
    expect(keyboardOpen({ ...base, innerHeight: 500, vvHeight: 500 })).toBe(false);
  });
});

describe('plainOutput', () => {
  test('no ANSI, no trailing blanks', () => {
    expect(plainOutput('\u001b[31mred\u001b[0m   \r\nline two\t\n\n\n')).toBe('red\nline two');
    expect(plainOutput('')).toBe('');
  });
});

describe('file line hand-over', () => {
  test('taken once, only for the requested path', () => {
    requestFileLine('src/a.ts', 42);
    expect(takeFileLine('src/b.ts')).toBeNull();
    expect(takeFileLine('src/a.ts')).toBe(42);
    expect(takeFileLine('src/a.ts')).toBeNull();
    requestFileLine('src/a.ts', undefined);
    expect(takeFileLine('src/a.ts')).toBeNull();
  });
});

describe('relative Markdown links', () => {
  test('kept as file links; schemes and anchors still drop out', () => {
    const b = parseMarkdown('[a](docs/x.md) [b](javascript:alert(1)) [c](#top) [d](//evil.test/x) [e](https://ok.test/)');
    const p = b[0] as { t: 'p'; c: any[] };
    const links = p.c.filter((n) => n.t === 'link');
    expect(links.map((n) => n.href)).toEqual(['docs/x.md', 'https://ok.test/']);
    expect(links[0].rel).toBe(true);
    expect(links[1].rel).toBeUndefined();
  });
});
