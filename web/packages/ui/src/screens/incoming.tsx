// Incoming handoffs (spec 16 §15.2): work another host sent to one of the user's hosts waits
// there until the receiver accepts it. The list shows every own host's handoffs; the accept view
// shows what arrived and lets the receiver choose where it lands: one of the matching clones, a
// folder they pick, or a fresh clone; the worktree path and branch; whether the agent resumes;
// and whether mise / direnv may be trusted in the new worktree.
//
// Folders are picked with `fs.browse` on the receiving host; the desktop app on that very host
// opens the native folder dialog instead.

import { useEffect, useState, type ReactNode } from 'react';
import { CircleAlert, FolderGit2, GitBranch, Inbox, KeyRound, Play, Server, Users } from 'lucide-react';
import { OutcomeUnknownError, RpcError, transportOf, type HostState, type IncomingHandoff } from '@vibeke/core';
import { useHandoffStores, useHostIncoming, useIncoming } from '../app/handoff-stores';
import { useAllHosts, useApp, useNow } from '../app/hooks';
import { PathPicker } from '../components/path-picker';
import { Button, Card, Chip, Empty, Notice, Row, SectionLabel, Spinner, TextField, Toggle, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { byteSize, whenText } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { isOwnFullHost } from '../lib/handoff-send';
import {
  acceptParams,
  importedTarget,
  initialForm,
  repoChoices,
  repoMismatch,
  resumable,
  skippedOther,
  skippedSecrets,
  sortIncoming,
  type AcceptForm,
  type FormProblem,
  type RepoMismatch,
  type Suggested,
} from '../lib/incoming';
import { navigate, workspaceRoute } from '../router';

/** Clones and imports can take a while (the host allows 10 min for a clone). */
const ACCEPT_TIMEOUT_MS = 15 * 60_000;

const hostName = (h: HostState): string => h.info?.host_name ?? h.record.name;

export function IncomingScreen({ host, id }: { host: string | null; id: string | null }) {
  if (host && id) return <AcceptView hostId={host} id={id} />;
  return <IncomingList only={host} />;
}

function IncomingList({ only }: { only: string | null }) {
  const hosts = useAllHosts().filter(isOwnFullHost);
  const incoming = useIncoming();
  const shown = only ? hosts.filter((h) => h.record.host_id === only) : hosts;
  const any = shown.some((h) => (incoming.get(h.record.host_id)?.list.length ?? 0) > 0);
  if (shown.length === 0) return <Empty icon={<Inbox />} title={t.incoming.empty} hint={t.incoming.noHosts} />;
  return (
    <div className="pb-10">
      {!any && <Empty icon={<Inbox />} title={t.incoming.empty} hint={t.incoming.emptyHint} />}
      {shown.map((h) => (
        <HostList key={h.record.host_id} h={h} showName={shown.length > 1} />
      ))}
    </div>
  );
}

function HostList({ h, showName }: { h: HostState; showName: boolean }) {
  const data = useHostIncoming(h.record.host_id);
  const now = useNow(60_000);
  const list = sortIncoming(data.list);
  if (!list.length && !data.error) return null;
  return (
    <section>
      {showName && <SectionLabel>{hostName(h)}</SectionLabel>}
      {data.error && (
        <div className="px-4 py-2">
          <Notice tone="danger">{data.error}</Notice>
        </div>
      )}
      <div className="space-y-px px-2">
        {list.map((r) => (
          <Row
            key={r.id}
            leading={r.from.owner === 'teammate' ? <Users className="size-4" /> : <Server className="size-4" />}
            trailing={<StateChip r={r} />}
            onClick={() => navigate({ name: 'handoffs', host: h.record.host_id, id: r.id })}
            sub={
              <>
                <GitBranch className="size-3 shrink-0" />
                <span className="truncate">{r.manifest.branch ?? r.manifest.head.slice(0, 10)}</span>
                <span className="text-faint">·</span>
                <span className="truncate">{t.incoming.fromAt(fromLabel(r), whenText(r.created_at_ms, now))}</span>
              </>
            }
          >
            <span className={cx(r.state === 'pending' || r.state === 'failed' ? 'font-medium text-fg' : '')}>{r.manifest.repo_name}</span>
          </Row>
        ))}
      </div>
    </section>
  );
}

const fromLabel = (r: IncomingHandoff): string => (r.from.user ? `${r.from.user} (${r.from.host})` : r.from.host);

function StateChip({ r }: { r: IncomingHandoff }) {
  const tone = r.state === 'pending' ? 'need' : r.state === 'failed' ? 'del' : r.state === 'imported' ? 'add' : r.state === 'importing' ? 'info' : 'default';
  return <Chip tone={tone}>{t.incoming.states[r.state] ?? r.state}</Chip>;
}

// ---- one handoff -------------------------------------------------------------------------------

type Loaded = { record: IncomingHandoff; suggested: Suggested };

function AcceptView({ hostId, id }: { hostId: string; id: string }) {
  const app = useApp();
  const stores = useHandoffStores();
  const host = useAllHosts().find((h) => h.record.host_id === hostId);
  const live = useHostIncoming(hostId);
  const [loaded, setLoaded] = useState<Loaded | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const online = host?.status === 'online';

  useEffect(() => {
    if (!online) return;
    const conn = app.conn(hostId);
    if (!conn) return;
    let alive = true;
    setErr(null);
    conn.request('handoff.incoming.get', { id }).then(
      (r) => alive && setLoaded({ record: r.incoming, suggested: r.suggested }),
      (e) => alive && setErr(errorMessage(e)),
    );
    return () => {
      alive = false;
    };
  }, [online, hostId, id]);

  if (!host || !isOwnFullHost(host)) return <Empty title={t.incoming.notFound} />;
  // The live copy (events) wins over the one fetched when the view opened.
  const record = live.list.find((r) => r.id === id) ?? loaded?.record ?? null;
  if (!record) {
    if (err) return <Empty icon={<CircleAlert />} title={t.incoming.notFound} hint={err} />;
    if (!online) return <Empty title={hostName(host)} hint={t.conn.hostOffline} />;
    return (
      <div className="flex justify-center py-16">
        <Spinner />
      </div>
    );
  }
  return (
    <div className="space-y-4 px-4 pb-10 pt-2">
      <Summary r={record} hostName={hostName(host)} />
      <Outcome r={record} host={host} phase={live.phase[record.id] ?? null} onRecord={(rec) => stores.putIncoming(hostId, rec)} />
      {(record.state === 'pending' || record.state === 'failed') && loaded && (
        <AcceptFormCard key={record.id} r={record} host={host} suggested={loaded.suggested} onRecord={(rec) => stores.putIncoming(hostId, rec)} />
      )}
      {(record.state === 'pending' || record.state === 'failed') && !loaded && !err && (
        <div className="flex justify-center py-6">
          <Spinner />
        </div>
      )}
    </div>
  );
}

function Summary({ r, hostName: to }: { r: IncomingHandoff; hostName: string }) {
  const now = useNow(60_000);
  const m = r.manifest;
  const secrets = skippedSecrets(r);
  const other = skippedOther(r);
  const [have, setHave] = useState<Record<string, boolean>>({});
  return (
    <Card className="space-y-3 p-4">
      <div className="flex items-start gap-3">
        {r.from.owner === 'teammate' ? <Users className="mt-0.5 size-5 text-muted" /> : <Server className="mt-0.5 size-5 text-muted" />}
        <div className="min-w-0 flex-1">
          <div className="text-base font-semibold">{t.incoming.title(fromLabel(r), to)}</div>
          <div className="text-xs text-muted">
            {[r.from.owner === 'teammate' ? t.incoming.teammate : t.incoming.ownHost, t.incoming.received(whenText(r.created_at_ms, now)), t.incoming.expires(whenText(r.expires_at_ms, now))].join(' · ')}
          </div>
        </div>
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
        <Item label={t.handoff.repo}>
          {m.repo_name}
          {m.origin && <div className="break-all font-mono text-2xs text-muted">{m.origin}</div>}
        </Item>
        <Item label={t.handoff.branch}>
          <span className="font-mono">{m.branch ?? m.head.slice(0, 10)}</span>
        </Item>
        {m.harness && <Item label={t.handoff.harness}>{harnessLabel(m.harness)}</Item>}
        <Item label={t.incoming.files}>
          {[m.untracked > 0 ? t.handoff.untracked(m.untracked) : t.incoming.noUntracked, byteSize(r.size)].join(' · ')}
        </Item>
      </dl>
      <div className={cx('text-sm', resumable(r) ? 'text-ok' : 'text-muted')}>{resumable(r) ? t.handoff.resumable : t.handoff.notResumable}</div>
      {m.last_message && (
        <div className="space-y-1">
          <div className="text-xs text-muted">{t.incoming.lastMessage}</div>
          <blockquote className="max-h-40 overflow-y-auto whitespace-pre-wrap rounded-lg border-l-2 border-border-strong bg-surface-2 px-3 py-2 text-sm">{m.last_message}</blockquote>
        </div>
      )}
      {secrets.length > 0 && (
        <Notice tone="warn">
          <div className="flex items-center gap-1.5 font-medium">
            <KeyRound className="size-4" />
            {t.incoming.secrets}
          </div>
          <ul className="mt-1 space-y-1">
            {secrets.map((p) => (
              <li key={p}>
                <label className="flex items-center gap-2 font-mono text-xs">
                  <input type="checkbox" checked={!!have[p]} onChange={(e) => setHave((x) => ({ ...x, [p]: e.target.checked }))} />
                  <span className={cx('break-all', have[p] && 'text-muted line-through')}>{p}</span>
                </label>
              </li>
            ))}
          </ul>
        </Notice>
      )}
      {other.length > 0 && (
        <div className="text-xs text-muted">
          <div>{t.handoff.skipped}</div>
          <ul className="font-mono">
            {other.map((x) => (
              <li key={x.path} className="break-all">
                {x.path} — {x.reason}
              </li>
            ))}
          </ul>
        </div>
      )}
      {m.redactions > 0 && <div className="text-xs text-muted">{t.handoff.redactions(m.redactions)}</div>}
    </Card>
  );
}

/** Importing / imported / declined / failed: what happened, and what can be done about it. */
function Outcome({ r, host, phase, onRecord }: { r: IncomingHandoff; host: HostState; phase: string | null; onRecord(r: IncomingHandoff): void }) {
  const app = useApp();
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const hostId = host.record.host_id;
  const open = (rec: IncomingHandoff) => openImported(hostId, rec);

  const resume = async () => {
    const conn = app.conn(hostId);
    if (!conn) return;
    setBusy(true);
    setErr(null);
    try {
      const out = await conn.request('handoff.resume', { id: r.id }, { timeoutMs: 120_000 });
      onRecord(out.incoming);
      if (out.agent_error) setErr(out.agent_error.message ?? out.agent_error.data?.kind ?? t.unknownError);
      else {
        app.haptic('success');
        open(out.incoming);
      }
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };

  switch (r.state) {
    case 'importing':
      return (
        <Notice>
          <div className="flex items-center gap-2">
            <Spinner />
            {phase ? (t.incoming.phases[phase] ?? phase) : t.incoming.importing}
          </div>
        </Notice>
      );
    case 'declined':
      return <Notice>{t.incoming.declinedNote}</Notice>;
    case 'failed':
      return <AcceptError error={r.error} />;
    case 'imported': {
      const res = r.result;
      const target = importedTarget(r);
      return (
        <Card className="space-y-3 p-4">
          <div className="font-medium text-ok">{t.incoming.imported}</div>
          {res && (
            <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
              {res.repo && <Item label={t.handoff.repo}><span className="break-all font-mono text-xs">{res.repo}</span></Item>}
              {res.worktree && <Item label={t.handoff.worktree}><span className="break-all font-mono text-xs">{res.worktree}</span></Item>}
              {res.branch && <Item label={t.handoff.newBranch}><span className="font-mono">{res.branch}</span></Item>}
            </dl>
          )}
          {(res?.not_written?.length ?? 0) > 0 && (
            <Notice tone="warn">
              <div>{t.incoming.notWritten}</div>
              <ul className="font-mono text-xs">
                {res!.not_written!.map((x) => (
                  <li key={x.path} className="break-all">
                    {x.path} — {x.reason}
                  </li>
                ))}
              </ul>
            </Notice>
          )}
          {res?.trust?.some((x) => x.status === 'failed') && <Notice tone="warn">{t.incoming.trustFailed(res!.trust!.filter((x) => x.status === 'failed').map((x) => x.tool).join(', '))}</Notice>}
          {res?.workspace_error && <Notice tone="warn">{res.workspace_error}</Notice>}
          {res?.agent_error && (
            <Notice tone="warn">
              {t.handoff.agentError} {res.agent_error.message ?? res.agent_error.data?.kind ?? ''}
            </Notice>
          )}
          {err && <Notice tone="danger">{err}</Notice>}
          <div className="flex flex-wrap gap-2">
            {target && (
              <Button variant="primary" onClick={() => open(r)}>
                {t.incoming.open}
              </Button>
            )}
            {res?.agent_error && (
              <Button variant="outline" busy={busy} icon={<Play className="size-4" />} onClick={() => void resume()}>
                {t.incoming.retryResume}
              </Button>
            )}
          </div>
        </Card>
      );
    }
    default:
      return null;
  }
}

function openImported(hostId: string, r: IncomingHandoff): void {
  const target = importedTarget(r);
  if (!target) return;
  if ('pane' in target) navigate({ name: 'pane', host: hostId, pane: target.pane, view: 'term' });
  else navigate(workspaceRoute(hostId, target.workspace));
}

function AcceptError({ error }: { error: unknown }) {
  if (!error) return null;
  const mm = repoMismatch(error);
  if (mm) return <MismatchNotice mm={mm} />;
  const msg = typeof error === 'object' && error !== null && 'message' in error ? String((error as { message: unknown }).message) : String(error);
  return (
    <Notice tone="danger">
      {t.incoming.failed} {msg}
    </Notice>
  );
}

function MismatchNotice({ mm }: { mm: RepoMismatch }) {
  return (
    <Notice tone="danger">
      <div>{t.incoming.mismatch(mm.repo, mm.origin)}</div>
      {mm.remotes.length > 0 ? (
        <ul className="mt-1 font-mono text-xs">
          {mm.remotes.map((r) => (
            <li key={r} className="break-all">
              {r}
            </li>
          ))}
        </ul>
      ) : (
        <div className="text-xs">{t.incoming.noRemotes}</div>
      )}
    </Notice>
  );
}

const PROBLEM_TEXT: Record<FormProblem, () => string> = {
  repo: () => t.incoming.problems.repo,
  folder: () => t.incoming.problems.folder,
  clone: () => t.incoming.problems.clone,
  worktree: () => t.incoming.problems.worktree,
  branch: () => t.incoming.problems.branch,
};

function AcceptFormCard({ r, host, suggested, onRecord }: { r: IncomingHandoff; host: HostState; suggested: Suggested; onRecord(r: IncomingHandoff): void }) {
  const app = useApp();
  const hostId = host.record.host_id;
  const [f, setF] = useState<AcceptForm>(() => initialForm(r, suggested));
  const [busy, setBusy] = useState<'accept' | 'decline' | null>(null);
  const [problem, setProblem] = useState<FormProblem | null>(null);
  const [err, setErr] = useState<{ message: string; mismatch: RepoMismatch | null } | null>(null);
  const [armedDecline, setArmedDecline] = useState(false);
  const patch = (p: Partial<AcceptForm>) => {
    setF((x) => ({ ...x, ...p }));
    setProblem(null);
  };
  const conn = app.conn(hostId);
  const online = host.status === 'online';
  const browse = (path: string, prefix?: string) => {
    if (!conn) return Promise.reject(new Error(t.conn.hostOffline));
    return conn.request('fs.browse', { path, ...(prefix ? { prefix } : {}) });
  };
  // The native folder dialog only shows this machine's folders.
  const pick = app.platform.pickDirectory;
  const pickNative = pick && transportOf(host.record.relay) === 'local' ? (start: string) => pick(start || undefined) : undefined;
  const choices = repoChoices(suggested, f.mode === 'existing' ? f.repo : '');
  const harness = r.manifest.harness;

  const accept = async () => {
    const built = acceptParams(r.id, f);
    if (!built.ok) return setProblem(built.problem);
    if (!conn) return;
    setBusy('accept');
    setErr(null);
    try {
      const out = await conn.request('handoff.accept', built.params, { timeoutMs: ACCEPT_TIMEOUT_MS });
      onRecord(out.incoming);
      const res = out.incoming.result;
      if (out.incoming.state === 'imported' && !res?.agent_error && !res?.workspace_error) {
        app.haptic('success');
        openImported(hostId, out.incoming);
      } else app.haptic(out.incoming.state === 'imported' ? 'warning' : 'error');
    } catch (e) {
      app.haptic('error');
      if (e instanceof OutcomeUnknownError) setErr({ message: t.incoming.unknown, mismatch: null });
      else setErr({ message: errorMessage(e), mismatch: e instanceof RpcError ? repoMismatch(e) : null });
    } finally {
      setBusy(null);
    }
  };

  const decline = async () => {
    if (!armedDecline) return setArmedDecline(true);
    if (!conn) return;
    setBusy('decline');
    setErr(null);
    try {
      const out = await conn.request('handoff.decline', { id: r.id });
      onRecord(out.incoming);
      navigate({ name: 'handoffs', host: null, id: null });
    } catch (e) {
      setErr({ message: errorMessage(e), mismatch: null });
    } finally {
      setBusy(null);
      setArmedDecline(false);
    }
  };

  const radio = (mode: AcceptForm['mode'], repo: string | null, label: ReactNode, sub?: ReactNode) => {
    const checked = f.mode === mode && (repo === null || f.repo === repo);
    return (
      <label className={cx('flex cursor-pointer items-start gap-2.5 rounded-lg border px-3 py-2', checked ? 'border-border-strong bg-selected' : 'border-border')}>
        <input type="radio" name={`repo-${r.id}`} className="mt-1" checked={checked} onChange={() => patch(repo === null ? { mode } : { mode, repo })} />
        <span className="min-w-0 flex-1">
          <span className="block text-sm">{label}</span>
          {sub && <span className="block text-xs text-muted">{sub}</span>}
        </span>
      </label>
    );
  };

  return (
    <Card className="space-y-4 p-4">
      <div className="text-base font-semibold">{t.incoming.whereTitle}</div>

      <fieldset className="space-y-2">
        <legend className="mb-1 text-xs text-muted">{t.incoming.repository}</legend>
        {choices.map((p) =>
          radio(
            'existing',
            p,
            <span className="flex items-center gap-1.5 break-all font-mono text-xs">
              <FolderGit2 className="size-3.5 shrink-0 text-muted" />
              {p}
            </span>,
            p === suggested.repo ? t.incoming.suggestedRepo : undefined,
          ),
        )}
        {choices.length === 0 && <div className="text-xs text-muted">{t.incoming.noClone}</div>}
        {radio('folder', null, t.incoming.chooseFolder, t.incoming.chooseFolderHint)}
        {f.mode === 'folder' && (
          <div className="pl-6">
            <PathPicker value={f.folder} onChange={(v) => patch({ folder: v })} browse={browse} pickNative={pickNative} label={t.incoming.folder} />
          </div>
        )}
        {r.manifest.origin && radio('clone', null, t.incoming.cloneTo, t.incoming.cloneHint(r.manifest.origin))}
        {f.mode === 'clone' && (
          <div className="pl-6">
            <PathPicker value={f.cloneTo} onChange={(v) => patch({ cloneTo: v })} browse={browse} pickNative={pickNative} label={t.incoming.cloneTarget} />
          </div>
        )}
        {problem && problem !== 'worktree' && problem !== 'branch' && <div className="text-xs text-danger">{PROBLEM_TEXT[problem]()}</div>}
      </fieldset>

      <div className="space-y-1">
        <PathPicker value={f.worktree} onChange={(v) => patch({ worktree: v })} browse={browse} pickNative={pickNative} label={t.incoming.worktree} placeholder={t.incoming.worktreePlaceholder} />
        <div className="text-xs text-muted">{t.incoming.worktreeHint}</div>
        {problem === 'worktree' && <div className="text-xs text-danger">{PROBLEM_TEXT.worktree()}</div>}
      </div>

      <div className="space-y-1">
        <TextField label={t.incoming.branch} value={f.branch} onChange={(e) => patch({ branch: e.target.value })} autoCapitalize="off" autoCorrect="off" spellCheck={false} className="font-mono" />
        {problem === 'branch' && <div className="text-xs text-danger">{PROBLEM_TEXT.branch()}</div>}
      </div>

      <div className="divide-y divide-border rounded-lg border border-border">
        <ToggleRow
          label={harness ? t.incoming.resume(harnessLabel(harness)) : t.incoming.resumeNone}
          hint={harness ? (resumable(r) ? t.handoff.resumable : t.handoff.notResumable) : undefined}
          checked={f.resume && !!harness}
          disabled={!harness}
          onChange={(v) => patch({ resume: v })}
        />
        <ToggleRow label={t.incoming.trustMise} hint={t.incoming.trustHint} checked={f.trustMise} onChange={(v) => patch({ trustMise: v })} />
        <ToggleRow label={t.incoming.trustDirenv} checked={f.trustDirenv} onChange={(v) => patch({ trustDirenv: v })} />
      </div>

      {err && (err.mismatch ? <MismatchNotice mm={err.mismatch} /> : <Notice tone="danger">{err.message}</Notice>)}
      {!online && <Notice tone="warn">{t.conn.hostOffline}</Notice>}
      {busy === 'accept' && f.mode === 'clone' && <div className="text-xs text-muted">{t.incoming.cloning}</div>}

      <div className="flex flex-wrap gap-2">
        <Button variant="primary" size="lg" busy={busy === 'accept'} disabled={!online || busy !== null} onClick={() => void accept()}>
          {r.state === 'failed' ? t.incoming.acceptAgain : t.incoming.accept}
        </Button>
        <Button variant={armedDecline ? 'danger' : 'ghost'} size="lg" busy={busy === 'decline'} disabled={!online || busy !== null} onClick={() => void decline()}>
          {armedDecline ? t.incoming.declineConfirm : t.incoming.decline}
        </Button>
      </div>
    </Card>
  );
}

function ToggleRow({ label, hint, checked, disabled, onChange }: { label: string; hint?: string; checked: boolean; disabled?: boolean; onChange(v: boolean): void }) {
  return (
    <div className="flex min-h-11 items-center gap-3 px-3 py-2">
      <div className="min-w-0 flex-1">
        <div className="text-sm">{label}</div>
        {hint && <div className="text-xs text-muted">{hint}</div>}
      </div>
      <Toggle label={label} checked={checked} disabled={disabled} onChange={onChange} />
    </div>
  );
}

function Item({ label, children }: { label: string; children: ReactNode }) {
  return (
    <>
      <dt className="text-muted">{label}</dt>
      <dd className="min-w-0">{children}</dd>
    </>
  );
}
