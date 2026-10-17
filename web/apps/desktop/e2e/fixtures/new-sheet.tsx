// The real NewSheet with a deterministic host API, including delayed replies for race checks.
import { useState } from 'react';
import { createRoot } from 'react-dom/client';
import type { BrowseResult, Worktree } from '@vibeke/core';
import { AppContext } from '../../../../packages/ui/src/app/hooks';
import type { AppModel } from '../../../../packages/ui/src/app/model';
import { NewSheet } from '../../../../packages/ui/src/components/new-sheet';
import { PrefsStore } from '../../../../packages/ui/src/lib/prefs';
import { ValueStore } from '../../../../packages/ui/src/lib/store';
import { dashboard, host, ws } from '../../../../packages/ui/test/fixtures';

const calls: { method: string; params: Record<string, unknown> }[] = [];
const held = new Map<string, (() => void)[]>();
const canonical = (input: string) => {
  let path = input === '~' ? '/host' : input.startsWith('~/') ? `/host/${input.slice(2)}` : input;
  while (path.length > 1 && path.endsWith('/')) path = path.slice(0, -1);
  return path || '/';
};
const dirs: Record<string, string[]> = {
  '/host': ['code'],
  '/host/code': ['repo', 'linked', 'scratch'],
  '/host/code/repo': ['docs'],
  '/host/code/repo/docs': [],
  '/host/code/linked': [],
  '/host/code/scratch': [],
  '/host/code/denied': [],
};
const worktrees: Worktree[] = [
  { path: '/host/code/repo', branch: 'main', head: 'abc', main: true, locked: false, prunable: false },
  { path: '/host/code/linked', branch: 'feature', head: 'abc', main: false, locked: false, prunable: false },
];
const prefs = new PrefsStore({ get: () => null, set: () => {}, remove: () => {} });
prefs.patch({ newAgentFolders: { h1: { favorites: ['~/code/linked', '~/code/scratch'], recent: [] } } });
const state = host('h1', dashboard({ workspaces: [ws({ name: 'workspace', root_path: '/host/code/repo', branch: 'main' })] }));
state.info!.features = ['agent_new_workspace'];
const manager = new ValueStore([state]);
const conn = {
  async request(method: string, params: Record<string, unknown>) {
    calls.push({ method, params });
    const key = `${method}:${canonical(String(params.path ?? params.cwd ?? ''))}`;
    const pending = held.get(key);
    if (pending) await new Promise<void>((resolve) => pending.push(resolve));
    if (method === 'agent.harnesses') return { harnesses: [{ id: 'claude', display: 'Claude Code', capabilities: [], version_detected: '1.0' }] };
    if (method === 'fs.browse') {
      const path = canonical(String(params.path));
      const entries = dirs[path];
      if (!entries) throw new Error(`no such folder: ${path}`);
      return {
        path, parent: path === '/host' ? null : path.slice(0, path.lastIndexOf('/')),
        git_repo: path === '/host/code/repo' || path === '/host/code/linked',
        entries: entries.map((name) => ({ name, git_repo: name === 'repo' || name === 'linked' })), truncated: false,
      } satisfies BrowseResult;
    }
    if (method === 'worktree.list') {
      if (params.cwd === '/host/code/scratch' || params.cwd === '/host') throw new Error('fatal: not a git repository');
      if (params.cwd === '/host/code/denied') throw new Error('Permission denied while checking Git');
      return { worktrees };
    }
    if (method === 'agent.start') return { pane: 'p9' };
    throw new Error(`Unexpected request: ${method}`);
  },
  refresh: async () => {},
  getSnapshot: () => state,
};
const app = { manager, prefs, cached: new ValueStore({}), conn: () => conn, haptic: () => {} } as unknown as AppModel;

declare global {
  interface Window {
    newSheetTest: {
      calls: typeof calls;
      hold(method: string, path: string): void;
      release(method: string, path: string): void;
      restrict(): void;
      reopen(): void;
    };
  }
}
window.newSheetTest = {
  calls,
  hold: (method, path) => { held.set(`${method}:${path}`, []); },
  release: (method, path) => {
    const key = `${method}:${path}`;
    const pending = held.get(key);
    held.delete(key);
    pending?.forEach((resolve) => resolve());
  },
  restrict: () => manager.set([{ ...state, info: { ...state.info!, limit: { workspace: 'w1' } } }]),
  reopen: () => {},
};
function Harness() {
  const [open, setOpen] = useState(true);
  window.newSheetTest.reopen = () => setOpen(true);
  return <AppContext.Provider value={app}><NewSheet open={open} onClose={() => setOpen(false)} hostId="h1" workspaceId="w1" /></AppContext.Provider>;
}
const root = document.createElement('div');
document.body.append(root);
createRoot(root).render(<Harness />);
