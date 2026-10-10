// New agent / new tab sheet (spec 16 §9.1 Home): host, where it runs (workspace, new folder, new
// worktree), harness, optional first prompt. The logic (branch names, folder shortcuts, "Again",
// start parameters, retry ids) is in lib/new-agent.ts.

import { useEffect, useRef, useState } from 'react';
import { Star } from 'lucide-react';
import { displayName, type HarnessInfo } from '@vibeke/core';
import { useApp, useHosts, usePrefs } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { harnessLabel } from '../lib/harness';
import {
  EMPTY_FOLDERS,
  branchProblem,
  buildStartParams,
  folderLabel,
  generateBranchName,
  newOpId,
  normalizeFolder,
  opIdFor,
  pushRecent,
  recentOnly,
  resolveAgain,
  toggleFavorite,
  type LastStart,
  type OpIdSlot,
  type Where,
} from '../lib/new-agent';
import { takeNewAgentPrefill } from '../lib/new-agent-prefill';
import { workspaceRoute, navigate } from '../router';
import { PathPicker } from './path-picker';
import { Button, Notice, Segmented, Sheet, Toggle, cx } from './ui';

type GitInfo = { state: 'idle' | 'loading' | 'none' } | { state: 'ready'; defaultBranch: string | null; currentBranch: string | null };

export function NewSheet({
  open,
  onClose,
  hostId,
  workspaceId,
  cwd,
}: {
  open: boolean;
  onClose(): void;
  hostId?: string;
  workspaceId?: string;
  /** The folder of the pane on screen: a new terminal in `workspaceId` starts there (like the TUI). */
  cwd?: string | null;
}) {
  const app = useApp();
  const prefs = usePrefs();
  const online = useHosts().filter((h) => h.status === 'online');
  const hosts = online.filter((h) => (h.info?.scope ?? h.record.scope) === 'full');
  const [mode, setMode] = useState<'agent' | 'tab'>('agent');
  const [host, setHost] = useState<string | null>(hostId ?? null);
  const [ws, setWs] = useState<string | null>(workspaceId ?? null);
  // A host folder typed or chosen for a new workspace; null = use the workspace chips.
  const [folder, setFolder] = useState<string | null>(null);
  const [picking, setPicking] = useState(false);
  const [harness, setHarness] = useState<string | null>(null);
  const [harnesses, setHarnesses] = useState<HarnessInfo[]>([]);
  const [prompt, setPrompt] = useState('');
  const [worktree, setWorktree] = useState(false);
  const [branch, setBranch] = useState('');
  const [baseChoice, setBaseChoice] = useState<'default' | 'current'>('default');
  const [git, setGit] = useState<GitInfo>({ state: 'idle' });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  // The id of the last submission, kept so a retry of the same start never creates a second agent.
  const opRef = useRef<OpIdSlot | null>(null);

  const h = hosts.find((x) => x.record.host_id === host) ?? hosts[0];
  const hid = h?.record.host_id;
  const workspaces = h?.dashboard?.workspaces ?? [];
  const wsId = ws && workspaces.some((w) => w.id === ws) ? ws : (workspaces[0]?.id ?? null);
  const wsRow = workspaces.find((w) => w.id === wsId);

  const limited = !!(h?.info?.limit && (h.info.limit.workspace || h.info.limit.pane)) || h?.info?.kind === 'share' || h?.record.kind === 'share';
  const hasFeature = !!h?.info?.features?.includes('agent_new_workspace');
  // Why new folders and worktrees are off on this host, or null.
  const whereBlock = limited ? t.newAgent.limitedFolder : h && !hasFeature ? t.newAgent.oldHost : null;

  const where: Where | null = folder !== null && !whereBlock ? { kind: 'folder', cwd: folder } : wsId ? { kind: 'workspace', id: wsId } : null;
  const folders = (hid && prefs.newAgentFolders[hid]) || EMPTY_FOLDERS;
  const again = hid && mode === 'agent' ? resolveAgain(prefs.lastStart[hid], workspaces, harnesses) : null;

  // A prompt handed over by another screen (shared content).
  useEffect(() => {
    if (!open) return;
    const p = takeNewAgentPrefill();
    if (p) {
      setMode('agent');
      setPrompt(p.prompt);
    }
  }, [open]);

  useEffect(() => {
    if (!open || !h) return;
    let live = true;
    app
      .conn(h.record.host_id)
      ?.request('agent.harnesses', {})
      .then((r) => {
        if (!live) return;
        setHarnesses(r.harnesses);
        setHarness((cur) => cur ?? r.harnesses.find((x) => x.version_detected)?.id ?? r.harnesses[0]?.id ?? null);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [open, h?.record.host_id]);

  // Branches of the chosen workspace, for "Start from" (also tells whether it is a Git repository).
  const gitWs = open && mode === 'agent' && !whereBlock && folder === null ? wsId : null;
  useEffect(() => {
    if (!gitWs || !hid) {
      setGit({ state: 'idle' });
      return;
    }
    let live = true;
    setGit({ state: 'loading' });
    setBaseChoice('default');
    app
      .conn(hid)
      ?.request('worktree.list', { workspace: gitWs })
      .then((r) => {
        if (!live) return;
        const def = r.worktrees.find((x) => x.main)?.branch ?? null;
        setGit({ state: 'ready', defaultBranch: def, currentBranch: wsRow?.branch ?? def });
      })
      .catch(() => live && setGit({ state: 'none' }));
    return () => {
      live = false;
    };
  }, [gitWs, hid]);

  const gitReady = git.state === 'ready';
  const worktreeOn = worktree && gitReady && folder === null && !whereBlock;
  const defaultBranch = git.state === 'ready' ? git.defaultBranch : null;
  const currentBranch = git.state === 'ready' ? git.currentBranch : null;
  const canChooseBase = !!defaultBranch && !!currentBranch && defaultBranch !== currentBranch;
  const base = canChooseBase && baseChoice === 'current' ? currentBranch : (defaultBranch ?? currentBranch);
  const problem = worktreeOn ? branchProblem(branch.trim()) : null;
  const folderMissing = folder !== null && !whereBlock && normalizeFolder(folder) === '';
  const harnessReady = !!harness && !!harnesses.find((x) => x.id === harness)?.version_detected;

  const setWorktreeOn = (on: boolean) => {
    setWorktree(on);
    if (on && !branch.trim()) setBranch(generateBranchName());
  };

  const applyAgain = (a: LastStart) => {
    setHarness(a.harness);
    setPicking(false);
    if (a.where.kind === 'workspace') {
      setFolder(null);
      setWs(a.where.id);
    } else {
      setFolder(a.where.cwd);
    }
    setWorktreeOn(a.worktree && a.where.kind === 'workspace');
  };

  const submit = async () => {
    if (!h || !hid || !where) return;
    const conn = app.conn(hid);
    if (!conn) return;
    setBusy(true);
    setError(null);
    try {
      let pane: string;
      let target = wsId;
      if (mode === 'agent') {
        if (!harness) return;
        const params = buildStartParams({ harness, prompt, where, worktree: worktreeOn ? { branch, ...(base ? { base } : {}) } : null });
        opRef.current = opIdFor(opRef.current, { hid, params }, newOpId);
        const r = await conn.request('agent.start', { ...params, op_id: opRef.current.id });
        pane = r.pane;
        const last: LastStart = { harness, where: where.kind === 'folder' ? { kind: 'folder', cwd: normalizeFolder(where.cwd) } : where, worktree: worktreeOn };
        const cur = app.prefs.get().newAgentFolders[hid] ?? EMPTY_FOLDERS;
        app.prefs.patch({
          lastStart: { ...app.prefs.get().lastStart, [hid]: last },
          ...(where.kind === 'folder' ? { newAgentFolders: { ...app.prefs.get().newAgentFolders, [hid]: { ...cur, recent: pushRecent(cur.recent, where.cwd) } } } : {}),
        });
        target = null;
      } else {
        const here = hid === hostId && wsId === workspaceId && cwd ? cwd : null;
        const params = { workspace: wsId!, ...(here ? { cwd: here } : {}) };
        opRef.current = opIdFor(opRef.current, { hid, params }, newOpId);
        const r = await conn.request('tab.create', { ...params, op_id: opRef.current.id });
        pane = r.root_pane.id;
      }
      opRef.current = null;
      app.haptic('success');
      onClose();
      setPrompt('');
      setWorktree(false);
      setBranch('');
      await conn.refresh().catch(() => {});
      // The new pane's workspace (a new folder or worktree makes a new workspace).
      target = conn.getSnapshot().dashboard?.panes.find((p) => p.id === pane)?.workspace ?? target;
      navigate(target ? workspaceRoute(hid, target, { pane }) : { name: 'pane', host: hid, pane, view: 'term' });
    } catch (e) {
      setError(errorMessage(e));
      app.haptic('error');
    } finally {
      setBusy(false);
    }
  };

  const toggleFav = () => {
    if (!hid || folder === null) return;
    const cur = prefs.newAgentFolders[hid] ?? EMPTY_FOLDERS;
    app.prefs.patch({ newAgentFolders: { ...prefs.newAgentFolders, [hid]: { ...cur, favorites: toggleFavorite(cur.favorites, folder) } } });
  };
  const browse = (path: string, prefix?: string) => {
    const conn = hid ? app.conn(hid) : undefined;
    if (!conn) return Promise.reject(new Error(t.conn.hostOffline));
    return conn.request('fs.browse', { path, ...(prefix ? { prefix } : {}) });
  };

  const folderChip = (cwd: string, fav: boolean) => ({ value: cwd, label: folderLabel(cwd), title: cwd, fav });
  const isFav = folder !== null && folders.favorites.includes(normalizeFolder(folder));
  const selectedFolder = folder !== null && !picking ? normalizeFolder(folder) : null;
  const wsChipValue = folder === null || whereBlock ? wsId : null;

  return (
    <Sheet open={open} onClose={onClose} title={t.newAgent.title}>
      <div className="space-y-4">
        <Segmented<'agent' | 'tab'>
          label={t.newAgent.title}
          value={mode}
          onChange={setMode}
          options={[
            { value: 'agent', label: t.newAgent.agent },
            { value: 'tab', label: t.newAgent.tab },
          ]}
        />
        {hosts.length === 0 && <Notice tone="warn">{online.length > 0 ? t.newAgent.viewOnly : t.composer.offline}</Notice>}
        {hosts.length > 1 && (
          <Picker
            label={t.newAgent.host}
            value={h?.record.host_id ?? null}
            options={hosts.map((x) => ({ value: x.record.host_id, label: x.info?.host_name ?? x.record.name }))}
            onChange={setHost}
          />
        )}
        {mode === 'agent' && again && (
          <div>
            <button
              type="button"
              onClick={() => applyAgain(again)}
              className="h-9 max-w-full truncate rounded-full border border-accent/50 bg-accent/10 px-3 text-sm text-fg"
            >
              {t.newAgent.againLabel(
                [
                  harnessLabel(again.harness),
                  again.where.kind === 'workspace' ? displayName(workspaces.find((w) => w.id === (again.where as { id: string }).id)!) : folderLabel(again.where.cwd),
                  ...(again.worktree ? [t.newAgent.worktree.toLowerCase()] : []),
                ].join(' · '),
              )}
            </button>
          </div>
        )}
        <Picker
          label={t.newAgent.workspace}
          value={wsChipValue}
          options={workspaces.map((w) => ({ value: w.id, label: displayName(w) }))}
          onChange={(id) => {
            setWs(id);
            setFolder(null);
            setPicking(false);
          }}
        />
        {mode === 'agent' && (
          <>
            {(
              <div>
                <div className="mb-1 text-sm text-muted">{t.newAgent.folder}</div>
                <div className="flex flex-wrap gap-1.5">
                  {[...folders.favorites.map((p) => folderChip(p, true)), ...recentOnly(folders).map((p) => folderChip(p, false))].map((o) => (
                    <button
                      key={o.value}
                      type="button"
                      title={o.title}
                      disabled={!!whereBlock}
                      onClick={() => {
                        setFolder(o.value);
                        setPicking(false);
                      }}
                      aria-pressed={selectedFolder === o.value}
                      className={cx(
                        'inline-flex h-9 items-center gap-1 rounded-full border px-3 text-sm disabled:opacity-50',
                        selectedFolder === o.value ? 'border-accent bg-accent/10 text-fg' : 'border-border text-muted',
                      )}
                    >
                      {o.fav && <Star aria-hidden className="size-3.5" />}
                      {o.label}
                    </button>
                  ))}
                  <button
                    type="button"
                    disabled={!!whereBlock}
                    onClick={() => {
                      setPicking(true);
                      setFolder((f) => f ?? '~/');
                    }}
                    aria-pressed={picking}
                    className={cx('h-9 rounded-full border px-3 text-sm disabled:opacity-50', picking ? 'border-accent bg-accent/10 text-fg' : 'border-border text-muted')}
                  >
                    {t.newAgent.newFolder}
                  </button>
                </div>
                {whereBlock && <div className="mt-1 text-xs text-muted">{whereBlock}</div>}
              </div>
            )}
            {picking && !whereBlock && (
              <div className="space-y-1.5">
                <PathPicker value={folder ?? ''} onChange={setFolder} browse={browse} label={t.newAgent.pickFolder} />
                <Button
                  size="md"
                  icon={<Star aria-hidden className={cx(isFav && 'fill-current')} />}
                  disabled={normalizeFolder(folder ?? '') === ''}
                  onClick={toggleFav}
                >
                  {isFav ? t.newAgent.removeFavorite : t.newAgent.addFavorite}
                </Button>
                {folderMissing && <div className="text-xs text-muted">{t.newAgent.folderRequired}</div>}
              </div>
            )}
            <div>
              <div className="flex min-h-11 items-center justify-between gap-3">
                <div className="min-w-0">
                  <div className="text-sm text-fg">{t.newAgent.worktree}</div>
                  <div className="text-xs text-muted">
                    {whereBlock ?? (folder !== null ? t.newAgent.notGit : git.state === 'none' ? t.newAgent.notGit : git.state === 'loading' ? t.newAgent.checkingGit : t.newAgent.worktreeHint)}
                  </div>
                </div>
                <Toggle checked={worktreeOn} onChange={setWorktreeOn} label={t.newAgent.worktree} disabled={!gitReady || folder !== null || !!whereBlock} />
              </div>
              {worktreeOn && (
                <div className="mt-2 space-y-3">
                  <label className="block">
                    <span className="mb-1 block text-sm text-muted">{t.newAgent.branch}</span>
                    <input
                      value={branch}
                      onChange={(e) => setBranch(e.target.value)}
                      autoCapitalize="off"
                      autoCorrect="off"
                      autoComplete="off"
                      spellCheck={false}
                      aria-invalid={!!problem}
                      className="h-11 w-full rounded-xl border border-border bg-bg px-3 font-mono text-base"
                    />
                    {problem && <span className="mt-1 block text-xs text-danger">{t.newAgent.branchProblem[problem]}</span>}
                  </label>
                  {canChooseBase && (
                    <Picker
                      label={t.newAgent.startFrom}
                      value={baseChoice}
                      options={[
                        { value: 'default', label: t.newAgent.defaultBranch(defaultBranch!) },
                        { value: 'current', label: t.newAgent.currentBranch(currentBranch!) },
                      ]}
                      onChange={(v) => setBaseChoice(v as 'default' | 'current')}
                    />
                  )}
                </div>
              )}
            </div>
            <Picker
              label={t.newAgent.harness}
              value={harness}
              disabledReason={t.newAgent.notInstalled}
              options={harnesses.map((x) => ({
                value: x.id,
                label: `${x.display || harnessLabel(x.id)}${x.version_detected ? ` ${x.version_detected}` : ` (${t.newAgent.notDetected})`}`,
                disabled: !x.version_detected,
              }))}
              onChange={setHarness}
            />
            {harnesses.length > 0 && !harnesses.some((x) => x.version_detected) && <Notice tone="warn">{t.newAgent.noHarness}</Notice>}
            <label className="block">
              <span className="mb-1 block text-sm text-muted">{t.newAgent.prompt}</span>
              <textarea
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                rows={3}
                className="w-full resize-none rounded-xl border border-border bg-bg px-3 py-2 text-base"
              />
            </label>
          </>
        )}
        {error && <Notice tone="danger">{error}</Notice>}
        <Button
          variant="primary"
          block
          size="lg"
          busy={busy}
          disabled={!h || !where || (mode === 'agent' && (!harnessReady || folderMissing || !!problem))}
          onClick={() => void submit()}
        >
          {mode === 'agent' ? t.newAgent.start : t.newAgent.create}
        </Button>
      </div>
    </Sheet>
  );
}

function Picker({
  label,
  value,
  options,
  onChange,
  disabledReason,
}: {
  label: string;
  value: string | null;
  options: { value: string; label: string; disabled?: boolean }[];
  onChange(v: string): void;
  /** Shown under the chips when an option is disabled. */
  disabledReason?: string;
}) {
  return (
    <div>
      <div className="mb-1 text-sm text-muted">{label}</div>
      <div className="flex flex-wrap gap-1.5">
        {options.map((o) => (
          <button
            key={o.value}
            type="button"
            disabled={o.disabled}
            onClick={() => onChange(o.value)}
            aria-pressed={value === o.value}
            className={cx(
              'h-9 rounded-full border px-3 text-sm disabled:opacity-50',
              value === o.value ? 'border-accent bg-accent/10 text-fg' : 'border-border text-muted',
            )}
          >
            {o.label}
          </button>
        ))}
      </div>
      {disabledReason && options.some((o) => o.disabled) && <div className="mt-1 text-xs text-muted">{disabledReason}</div>}
    </div>
  );
}
