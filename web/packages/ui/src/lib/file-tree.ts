// Changed-files tree for the right panel: flat `path`s → nested directories with rolled-up
// +/− counts. Chains of single-child directories collapse into one row (`src/app`), directories
// sort before files, names compare case-insensitively with a stable tiebreak.

export interface TreeInput {
  path: string;
  adds?: number | null;
  dels?: number | null;
}

export interface FileNode<T extends TreeInput> {
  kind: 'file';
  /** Last path segment. */
  name: string;
  path: string;
  file: T;
  adds: number;
  dels: number;
}

export interface DirNode<T extends TreeInput> {
  kind: 'dir';
  /** Display name; compressed chains join with `/` (`packages/website/src`). */
  name: string;
  /** Full directory path (no trailing slash): the collapse key. */
  path: string;
  children: TreeNode<T>[];
  adds: number;
  dels: number;
  /** Files below this directory. */
  files: number;
}

export type TreeNode<T extends TreeInput> = FileNode<T> | DirNode<T>;

const byName = (a: { name: string; kind: string }, b: { name: string; kind: string }): number => {
  if (a.kind !== b.kind) return a.kind === 'dir' ? -1 : 1;
  const x = a.name.toLowerCase();
  const y = b.name.toLowerCase();
  if (x !== y) return x < y ? -1 : 1;
  return a.name < b.name ? -1 : a.name > b.name ? 1 : 0;
};

/** Build the tree. `compress: false` keeps every directory level (tests, deep links). */
export function buildFileTree<T extends TreeInput>(files: readonly T[], o: { compress?: boolean } = {}): TreeNode<T>[] {
  const root: DirNode<T> = { kind: 'dir', name: '', path: '', children: [], adds: 0, dels: 0, files: 0 };
  const dirs = new Map<string, DirNode<T>>([['', root]]);
  const dirFor = (path: string): DirNode<T> => {
    const hit = dirs.get(path);
    if (hit) return hit;
    const i = path.lastIndexOf('/');
    const parent = dirFor(i < 0 ? '' : path.slice(0, i));
    const d: DirNode<T> = { kind: 'dir', name: i < 0 ? path : path.slice(i + 1), path, children: [], adds: 0, dels: 0, files: 0 };
    parent.children.push(d);
    dirs.set(path, d);
    return d;
  };
  for (const f of files) {
    const path = f.path.replace(/\/+$/, '');
    if (!path) continue;
    const i = path.lastIndexOf('/');
    const dir = dirFor(i < 0 ? '' : path.slice(0, i));
    dir.children.push({ kind: 'file', name: i < 0 ? path : path.slice(i + 1), path, file: f, adds: f.adds ?? 0, dels: f.dels ?? 0 });
  }
  const finish = (d: DirNode<T>): void => {
    d.adds = 0;
    d.dels = 0;
    d.files = 0;
    for (const c of d.children) {
      if (c.kind === 'dir') {
        finish(c);
        d.files += c.files;
      } else d.files += 1;
      d.adds += c.adds;
      d.dels += c.dels;
    }
    d.children.sort(byName);
  };
  finish(root);
  if (o.compress !== false) {
    const squash = (n: TreeNode<T>): TreeNode<T> => {
      if (n.kind === 'file') return n;
      let d = n;
      while (d.children.length === 1 && d.children[0]!.kind === 'dir') {
        const only = d.children[0] as DirNode<T>;
        d = { ...only, name: `${d.name}/${only.name}` };
      }
      return { ...d, children: d.children.map(squash) };
    };
    root.children = root.children.map(squash);
    root.children.sort(byName);
  }
  return root.children;
}

export interface VisibleRow<T extends TreeInput> {
  node: TreeNode<T>;
  depth: number;
  /** Directory expanded (dirs only). */
  open: boolean;
}

/** Rows to render in order, skipping the children of collapsed directories. */
export function visibleRows<T extends TreeInput>(nodes: readonly TreeNode<T>[], collapsed: ReadonlySet<string>, depth = 0, out: VisibleRow<T>[] = []): VisibleRow<T>[] {
  for (const n of nodes) {
    const open = n.kind === 'dir' && !collapsed.has(n.path);
    out.push({ node: n, depth, open });
    if (n.kind === 'dir' && open) visibleRows(n.children, collapsed, depth + 1, out);
  }
  return out;
}

/** File paths in tree order (prev/next file follows what the user sees). */
export function fileOrder<T extends TreeInput>(nodes: readonly TreeNode<T>[], out: string[] = []): string[] {
  for (const n of nodes) {
    if (n.kind === 'file') out.push(n.path);
    else fileOrder(n.children, out);
  }
  return out;
}

/** Every directory path (expand / collapse all). */
export function dirPaths<T extends TreeInput>(nodes: readonly TreeNode<T>[], out: string[] = []): string[] {
  for (const n of nodes) {
    if (n.kind === 'dir') {
      out.push(n.path);
      dirPaths(n.children, out);
    }
  }
  return out;
}

/** Header totals: summed +/− over the files (binary files count 0). */
export function totals(files: readonly TreeInput[]): { adds: number; dels: number; files: number } {
  let adds = 0;
  let dels = 0;
  for (const f of files) {
    adds += f.adds ?? 0;
    dels += f.dels ?? 0;
  }
  return { adds, dels, files: files.length };
}

/** `fs.list` entries in tree order: folders first, then names (same comparison as above). */
export function sortEntries<E extends { name: string; kind: string }>(entries: readonly E[]): E[] {
  return [...entries].sort((a, b) => byName({ name: a.name, kind: a.kind === 'dir' ? 'dir' : 'file' }, { name: b.name, kind: b.kind === 'dir' ? 'dir' : 'file' }));
}
