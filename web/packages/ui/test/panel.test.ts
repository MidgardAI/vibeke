import { describe, expect, test } from 'bun:test';
import { buildFileTree, dirPaths, fileOrder, sortEntries, totals, visibleRows, type DirNode, type TreeInput, type TreeNode } from '../src/lib/file-tree';
import { formatRoute, parseRoute, workspaceRoute, type WorkspaceRoute } from '../src/router';
import {
  EMPTY_TREE,
  baseCandidates,
  baseRoute,
  closeFileRoute,
  commitRange,
  commitRoute,
  diffSource,
  fileDiffParams,
  fileRoute,
  listParams,
  showsCentreDiff,
} from '../src/screens/workspace/panel/routes';

const f = (path: string, adds = 0, dels = 0): TreeInput => ({ path, adds, dels });
const shape = (nodes: readonly TreeNode<TreeInput>[]): unknown =>
  nodes.map((n) => (n.kind === 'dir' ? { [`${n.name}/ +${n.adds} -${n.dels}`]: shape(n.children) } : `${n.name} +${n.adds} -${n.dels}`));

describe('buildFileTree', () => {
  test('compresses single-child directory chains and rolls up counts', () => {
    const tree = buildFileTree([
      f('packages/website/src/components/mockup/atoms.tsx', 56, 18),
      f('packages/website/src/components/mockup/chat.tsx', 155, 74),
      f('packages/website/src/routes/index.tsx', 44, 70),
      f('packages/website/src/hero.tsx', 89, 52),
    ]);
    expect(shape(tree)).toEqual([
      {
        'packages/website/src/ +344 -214': [
          { 'components/mockup/ +211 -92': ['atoms.tsx +56 -18', 'chat.tsx +155 -74'] },
          { 'routes/ +44 -70': ['index.tsx +44 -70'] },
          'hero.tsx +89 -52',
        ],
      },
    ]);
    const top = tree[0] as DirNode<TreeInput>;
    expect(top.path).toBe('packages/website/src');
    expect(top.files).toBe(4);
    // The compressed node's collapse key is its deepest directory.
    expect((top.children[0] as DirNode<TreeInput>).path).toBe('packages/website/src/components/mockup');
  });

  test('a directory holding a file and a folder is not compressed', () => {
    const tree = buildFileTree([f('a/b/c.ts', 1), f('a/d.ts', 2)]);
    expect(shape(tree)).toEqual([{ 'a/ +3 -0': [{ 'b/ +1 -0': ['c.ts +1 -0'] }, 'd.ts +2 -0'] }]);
    expect(shape(buildFileTree([f('x/y/z.ts')], { compress: false }))).toEqual([{ 'x/ +0 -0': [{ 'y/ +0 -0': ['z.ts +0 -0'] }] }]);
  });

  test('directories first, case-insensitive names, stable for equal keys', () => {
    const tree = buildFileTree([f('b.ts'), f('README.md'), f('src/z.ts'), f('a.ts'), f('Docs/x.md'), f('B.ts')]);
    expect(tree.map((n) => n.name)).toEqual(['Docs', 'src', 'a.ts', 'B.ts', 'b.ts', 'README.md']);
    expect(fileOrder(tree)).toEqual(['Docs/x.md', 'src/z.ts', 'a.ts', 'B.ts', 'b.ts', 'README.md']);
  });

  test('missing counts (binary) count as zero; trailing slashes are ignored', () => {
    const tree = buildFileTree([{ path: 'img/logo.png', adds: null, dels: null }, f('img/a.svg', 3, 1), f('dir/')]);
    expect(shape(tree)).toEqual([{ 'img/ +3 -1': ['a.svg +3 -1', 'logo.png +0 -0'] }, 'dir +0 -0']);
  });

  test('visible rows skip collapsed folders; dirPaths lists collapse keys', () => {
    const tree = buildFileTree([f('src/app/a.ts'), f('src/app/b.ts'), f('src/lib/c.ts'), f('top.ts')]);
    const all = visibleRows(tree, new Set());
    expect(all.map((r) => `${'  '.repeat(r.depth)}${r.node.name}`)).toEqual(['src', '  app', '    a.ts', '    b.ts', '  lib', '    c.ts', 'top.ts']);
    const some = visibleRows(tree, new Set(['src/app']));
    expect(some.map((r) => r.node.name)).toEqual(['src', 'app', 'lib', 'c.ts', 'top.ts']);
    expect(some[1]!.open).toBe(false);
    expect(dirPaths(tree)).toEqual(['src', 'src/app', 'src/lib']);
  });
});

describe('changes header totals', () => {
  test('sum adds and dels over files', () => {
    expect(totals([f('a', 1900, 600), f('b', 0, 84), { path: 'c.bin', adds: null, dels: null }])).toEqual({ adds: 1900, dels: 684, files: 3 });
    expect(totals([])).toEqual({ adds: 0, dels: 0, files: 0 });
  });
});

describe('panel routes', () => {
  const r: WorkspaceRoute = { ...workspaceRoute('h1', 'w1'), panel: 'changes' };

  test('selecting a commit clears file and base; deselecting returns to the changes', () => {
    const c = commitRoute({ ...r, file: 'a.ts', base: 'main' }, 'abc123');
    expect(c).toMatchObject({ commit: 'abc123', file: null, base: null, view: null, panel: 'changes' });
    expect(formatRoute(c)).toBe('#/w/h1/w1?panel=changes&commit=abc123');
    expect(parseRoute(formatRoute(c))).toMatchObject({ commit: 'abc123', file: null });
    expect(commitRoute(c, null).commit).toBeNull();
    // A file inside the commit keeps the commit.
    const cf = fileRoute(c, 'src/a.ts');
    expect(parseRoute(formatRoute(cf))).toMatchObject({ commit: 'abc123', file: 'src/a.ts', view: null });
    expect(closeFileRoute(cf)).toMatchObject({ commit: 'abc123', file: null });
  });

  test('a commit diffs against its parent (the empty tree for a root commit)', () => {
    expect(commitRange('abc')).toBe('abc^..abc');
    expect(commitRange('abc', true)).toBe(`${EMPTY_TREE}..abc`);
    const src = diffSource({ commit: 'abc', base: null });
    expect(src).toEqual({ kind: 'commit', sha: 'abc', range: 'abc^..abc' });
    expect(listParams('p1', src)).toEqual({ pane: 'p1', range: 'abc^..abc' });
    expect(fileDiffParams('p1', src, 'a.ts')).toEqual({ pane: 'p1', range: 'abc^..abc', file: 'a.ts' });
  });

  test('base and working-tree sources', () => {
    const b = diffSource({ commit: null, base: 'origin/main' });
    expect(listParams('p1', b)).toEqual({ pane: 'p1', base: 'origin/main' });
    expect(fileDiffParams('p1', b, 'x')).toEqual({ pane: 'p1', base: 'origin/main', file: 'x' });
    const w = diffSource({ commit: null, base: null });
    expect(listParams('p1', w)).toBeNull();
    expect(fileDiffParams('p1', w, 'x')).toEqual({ pane: 'p1', file: 'x' });
    expect(formatRoute(baseRoute({ ...r, commit: 'abc', file: 'x' }, 'origin/main'))).toBe('#/w/h1/w1?panel=changes&base=origin%2Fmain');
  });

  test('alt-click opens the diff in the centre', () => {
    const c = fileRoute(r, 'src/a.ts', { centre: true });
    expect(formatRoute(c)).toBe('#/w/h1/w1?panel=changes&file=src%2Fa.ts&view=diff');
    expect(showsCentreDiff(parseRoute(formatRoute(c)) as WorkspaceRoute)).toBe(true);
    expect(showsCentreDiff(fileRoute(r, 'src/a.ts'))).toBe(false);
  });

  test('base candidates: task base, then upstream, never the branch itself', () => {
    expect(baseCandidates({ branch: 'feat/x', upstream: 'origin/feat/x' }, { base_ref: 'main' })).toEqual(['main', 'origin/feat/x']);
    expect(baseCandidates({ branch: 'main', upstream: null }, { base_ref: 'main' })).toEqual([]);
    expect(baseCandidates(null, null)).toEqual([]);
  });
});

describe('files tree', () => {
  test('folders first, then names', () => {
    const e = (name: string, kind: 'file' | 'dir') => ({ name, kind, ignored: false, secret: false });
    expect(sortEntries([e('b.ts', 'file'), e('src', 'dir'), e('A.md', 'file'), e('docs', 'dir')]).map((x) => x.name)).toEqual(['docs', 'src', 'A.md', 'b.ts']);
  });
});
