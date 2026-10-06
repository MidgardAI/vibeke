import { describe, expect, test } from 'bun:test';
import { buildFileTree, dirPaths, fileOrder, sortEntries, totals, visibleRows, type DirNode, type TreeInput, type TreeNode } from '../src/lib/file-tree';
import { formatRoute, parseRoute, workspaceRoute, type WorkspaceRoute } from '../src/router';
import {
  EMPTY_TREE,
  baseCandidates,
  baseRoute,
  closeFileRoute,
  RefreshRevision,
  commitRange,
  commitRoute,
  diffReload,
  diffSource,
  isKnownRootCommit,
  listReload,
  markRootCommit,
  onRootCommits,
  resolvedSource,
  rootFallback,
  fileDiffParams,
  fileRoute,
  listParams,
  showsCentreDiff,
} from '../src/screens/workspace/panel/routes';
import { revFile, workFile } from '../src/screens/workspace/panel/changes-data';

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

describe('change files', () => {
  test('untracked text files carry line counts into the rows and the header total', () => {
    const files = [
      workFile({ path: 'src/a.ts', x: '.', y: 'M', kind: 'modified', adds: 2, dels: 1, binary: false }),
      workFile({ path: 'src/new.ts', x: '?', y: '?', kind: 'untracked', adds: 18, dels: 0, binary: false }),
      workFile({ path: 'img.png', x: '?', y: '?', kind: 'untracked', adds: null, dels: null, binary: true }),
    ];
    expect(files[1]).toMatchObject({ letter: '?', adds: 18, dels: 0, binary: false });
    expect(files[2]).toMatchObject({ letter: '?', binary: true });
    expect(totals(files)).toEqual({ adds: 20, dels: 1, files: 3 });
    const tree = buildFileTree(files);
    expect(shape(tree)).toEqual([{ 'src/ +20 -1': ['a.ts +2 -1', 'new.ts +18 -0'] }, 'img.png +0 -0']);
  });

  test('commit / base listings carry status letters and rename sources', () => {
    expect(revFile({ path: 'b.ts', adds: 1, dels: 1, binary: false, status: 'R', orig_path: 'a.ts' })).toMatchObject({ letter: 'R', orig_path: 'a.ts' });
    expect(revFile({ path: 'c.ts', adds: 3, dels: 0, binary: false, status: 'A', orig_path: null })).toMatchObject({ letter: 'A', orig_path: null });
    expect(revFile({ path: 'd.ts', adds: 0, dels: 4, binary: false, status: 'D' }).letter).toBe('D');
    for (const st of ['M', 'C', 'T'] as const) expect(revFile({ path: 'x', binary: false, status: st }).letter).toBe(st);
    // Older hosts send no status: no square.
    expect(revFile({ path: 'e.ts', adds: 1, dels: 0, binary: false })).toMatchObject({ letter: null, orig_path: null });
  });
});

describe('refresh revisions', () => {
  test('every new status answer moves the revision, even with the same counts; so does a manual refresh', () => {
    const rev = new RefreshRevision();
    const s1 = { files: [{ path: 'a', adds: 1, dels: 0 }] };
    const s2 = { files: [{ path: 'a', adds: 1, dels: 0 }] }; // same names and counts, new answer
    expect(rev.next(null, 0)).toBe(0);
    expect(rev.next(null, 0)).toBe(0); // re-render: unchanged
    expect(rev.next(s1, 0)).toBe(1);
    expect(rev.next(s1, 0)).toBe(1);
    expect(rev.next(s2, 0)).toBe(2);
    expect(rev.next(s2, 1)).toBe(3); // manual refresh
  });

  test('base listings and open diffs follow the revision; commits only a manual refresh', () => {
    const work = diffSource({ commit: null, base: null });
    const base = diffSource({ commit: null, base: 'main' });
    const commit = diffSource({ commit: 'abc', base: null });
    expect(listReload(work, 5, 1)).toBeNull();
    expect(listReload(base, 5, 1)).toBe(5);
    expect(listReload(commit, 5, 1)).toBe(1);
    expect(diffReload(work, 5, 1)).toBe(5);
    expect(diffReload(base, 5, 1)).toBe(5); // a base comparison's open diff refetches too
    expect(diffReload(commit, 5, 1)).toBe(1);
  });
});

describe('root commits', () => {
  test('known roots resolve to the empty tree for every view (panel, centre, direct route)', () => {
    let changes = 0;
    const off = onRootCommits(() => changes++);
    const route = { commit: 'r00t', base: null };
    expect(resolvedSource('h1', 'p1', route)).toEqual({ kind: 'commit', sha: 'r00t', range: 'r00t^..r00t' });
    expect(resolvedSource('h1', null, route).kind).toBe('commit');
    markRootCommit('h1', 'p1', 'r00t');
    markRootCommit('h1', 'p1', 'r00t'); // once
    expect(changes).toBe(1);
    expect(isKnownRootCommit('h1', 'p1', 'r00t')).toBe(true);
    expect(isKnownRootCommit('h1', 'p2', 'r00t')).toBe(false);
    const src = resolvedSource('h1', 'p1', route);
    expect(src).toEqual({ kind: 'commit', sha: 'r00t', range: `${EMPTY_TREE}..r00t` });
    expect(fileDiffParams('p1', src, 'README.md')).toEqual({ pane: 'p1', range: `${EMPTY_TREE}..r00t`, file: 'README.md' });
    expect(resolvedSource('h1', 'p1', { commit: null, base: 'main' })).toEqual({ kind: 'base', base: 'main' });
    off();
  });

  test('a failing parent range falls back to the empty tree once', () => {
    const src = diffSource({ commit: 'abc', base: null });
    const alt = rootFallback(src)!;
    expect(alt).toEqual({ kind: 'commit', sha: 'abc', range: `${EMPTY_TREE}..abc` });
    expect(rootFallback(alt)).toBeNull();
    expect(rootFallback(diffSource({ commit: null, base: 'main' }))).toBeNull();
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
