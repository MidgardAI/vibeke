import { describe, expect, test } from 'bun:test';
import { JSON_CAP, buildJsonTree, canPreview, countValues, imageDataUrl, isImagePath, isRasterMime, parseJson, previewKind, resolveRelativeLink } from '../src/lib/preview';

describe('previewKind', () => {
  test('by extension', () => {
    expect(previewKind('docs/README.md')).toBe('markdown');
    expect(previewKind('a/b.MARKDOWN')).toBe('markdown');
    expect(previewKind('logo.svg')).toBe('svg');
    expect(previewKind('package.json')).toBe('json');
    expect(previewKind('photo.png')).toBeNull();
    expect(previewKind('Makefile')).toBeNull();
  });
  test('a cut-off file previews only as Markdown', () => {
    expect(canPreview('markdown', true)).toBe(true);
    expect(canPreview('json', true)).toBe(false);
    expect(canPreview('svg', false)).toBe(true);
  });
});

describe('resolveRelativeLink', () => {
  test('relative to the file, fragments and queries dropped', () => {
    expect(resolveRelativeLink('docs/guide/intro.md', './setup.md#top')).toBe('docs/guide/setup.md');
    expect(resolveRelativeLink('docs/guide/intro.md', '../api.md?x=1')).toBe('docs/api.md');
    expect(resolveRelativeLink('README.md', 'src/a.ts')).toBe('src/a.ts');
    expect(resolveRelativeLink('docs/a.md', '/LICENSE')).toBe('LICENSE');
    expect(resolveRelativeLink('docs/a.md', 'my%20file.md')).toBe('docs/my file.md');
  });
  test('urls, anchors and escapes are not file links', () => {
    expect(resolveRelativeLink('a.md', 'https://example.com/x.md')).toBeNull();
    expect(resolveRelativeLink('a.md', 'mailto:me@example.com')).toBeNull();
    expect(resolveRelativeLink('a.md', '//cdn.example.com/x')).toBeNull();
    expect(resolveRelativeLink('a.md', '#section')).toBeNull();
    expect(resolveRelativeLink('a.md', '../../etc/passwd')).toBeNull();
    expect(resolveRelativeLink('a.md', 'a\\b')).toBeNull();
  });
});

describe('json tree', () => {
  test('parse', () => {
    expect(parseJson('{"a":1}')).toEqual({ ok: true, value: { a: 1 } });
    expect(parseJson('{oops')).toEqual({ ok: false });
  });
  test('counts values, containers included', () => {
    expect(countValues({ a: [1, 2], b: 'x' })).toBe(5);
    expect(countValues(null)).toBe(1);
  });
  test('small documents are complete', () => {
    const tree = buildJsonTree({ name: 'x', tags: ['a', 'b'], ok: true, none: null });
    expect(tree.capped).toBe(false);
    expect(tree.shown).toBe(tree.total);
    expect(tree.root).toMatchObject({ kind: 'object', size: 4, omitted: 0 });
    const kinds = tree.root.kind === 'object' ? tree.root.children.map((c) => c.kind) : [];
    expect(kinds).toEqual(['string', 'array', 'boolean', 'null']);
  });
  test('capped at the value limit, with the rest counted', () => {
    const big = Array.from({ length: 20_000 }, (_, i) => ({ i }));
    const tree = buildJsonTree(big);
    expect(tree.capped).toBe(true);
    expect(tree.total).toBe(1 + 20_000 * 2);
    expect(tree.shown).toBeLessThanOrEqual(JSON_CAP + 1);
    expect(tree.root.kind === 'array' && tree.root.omitted > 0).toBe(true);
    expect(tree.root.kind === 'array' && tree.root.size).toBe(20_000);
  });
  test('long strings are shortened and deep nesting is bounded', () => {
    const tree = buildJsonTree({ s: 'x'.repeat(1000) });
    const s = tree.root.kind === 'object' ? tree.root.children[0]! : null;
    expect(s && s.kind === 'string' && s.text.length).toBeLessThan(400);
    let deep: unknown = 1;
    for (let i = 0; i < 500; i++) deep = [deep];
    expect(() => buildJsonTree(deep)).not.toThrow();
  });
});

describe('images', () => {
  test('image paths are raster files only', () => {
    expect(isImagePath('a/shot.PNG')).toBe(true);
    expect(isImagePath('x.jpeg')).toBe(true);
    expect(isImagePath('x.webp')).toBe(true);
    expect(isImagePath('logo.svg')).toBe(false);
    expect(isImagePath('png')).toBe(false);
    expect(isImagePath('a.txt')).toBe(false);
  });

  test('data URLs need a raster type and clean base64', () => {
    expect(isRasterMime('image/svg+xml')).toBe(false);
    expect(imageDataUrl('image/png', 'iVBORw0KGgo=')).toBe('data:image/png;base64,iVBORw0KGgo=');
    expect(imageDataUrl('IMAGE/GIF', 'R0lGOD')).toBe('data:image/gif;base64,R0lGOD');
    expect(imageDataUrl('image/svg+xml', 'AAAA')).toBeNull();
    expect(imageDataUrl('image/png', 'AAAA" onerror="x')).toBeNull();
    expect(imageDataUrl('image/png', '')).toBeNull();
    expect(imageDataUrl(null, 'AAAA')).toBeNull();
  });
});
