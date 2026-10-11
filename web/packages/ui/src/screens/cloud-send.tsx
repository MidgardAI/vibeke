// Move work to and from a cloud sandbox (spec 17 §7). One sheet, two modes:
//
// - send: pick a provider (sign in when it needs it), pick a new or an existing sandbox,
//   confirm, then follow the `cloud.move` job.
// - bring back: pick where the work goes (this host, or a peer from `handoff.peers`), then
//   follow the job.
//
// The host runs the job, so the sheet can close at any point and the move goes on; a toast
// reports the end. Nothing mutating is retried by the app, except the one retry after sign-in:
// either the `cloud.move` call or its job failed with `needs_auth` (a token revoked meanwhile).

import { useEffect, useMemo, useRef, useState } from 'react';
import { ArrowRight, Check, CheckCircle2, CircleAlert, Cloud, OctagonX, Server, Box } from 'lucide-react';
import { OutcomeUnknownError, isHandoffBusy, type CloudBox, type CloudJob, type CloudMoveTarget, type CloudProvider, type CloudReview, type HandoffPeer } from '@vibeke/core';
import { useCloudStores, useHostCloud } from '../app/cloud-stores';
import { useApp, useHost } from '../app/hooks';
import { requestCloudAuth } from '../components/cloud-auth';
import { Button, Notice, Sheet, Spinner } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { cloudJobError, cloudJobFinal, cloudJobNeedsAuth, cloudJobPct, paneTaskKeys, runWithCloudAuth, sendBoxChoices, sharedCheckout } from '../lib/cloud';
import { navigate } from '../router';

export type CloudSheetProps =
  | { mode: 'send'; host: string; pane: string; open: boolean; onClose(): void; job?: string }
  | { mode: 'bring_back'; host: string; pane?: string; box?: string; open: boolean; onClose(): void; job?: string };

type Step =
  | { k: 'loading' }
  | { k: 'provider' }
  | { k: 'box'; provider: CloudProvider }
  | { k: 'confirm'; provider: CloudProvider; box: string | null }
  | { k: 'pickbox' }
  | { k: 'target'; box: string | null }
  | { k: 'starting' }
  | { k: 'busy'; retry: () => void }
  /** The pane shares its checkout: offer to copy the work into a new task and send that. */
  | { k: 'shared'; message: string; retry: () => void }
  | { k: 'job'; job: string }
  | { k: 'unknown' }
  | { k: 'error'; message: string; retry: (() => void) | null };

const JOB_POLL_MS = 3000;

type MoveParams = { to: CloudMoveTarget; box?: string; interrupt?: boolean; source_after?: 'keep' | 'suspend' | 'destroy'; new_task?: boolean };

export function CloudSheet(props: CloudSheetProps) {
  const { host, open, onClose } = props;
  const app = useApp();
  const stores = useCloudStores();
  const cloud = useHostCloud(host);
  const [step, setStep] = useState<Step>({ k: 'loading' });
  const [peers, setPeers] = useState<HandoffPeer[]>([]);
  const gen = useRef(0);
  const send = props.mode === 'send';
  const dashboard = useHost(host)?.dashboard;
  const taskKeys = useMemo(() => (send && props.pane ? paneTaskKeys(dashboard, props.pane) : []), [send, props.pane, dashboard]);
  /** The move the sheet started last, and whether it already had its one retry after sign-in. */
  const last = useRef<{ params: MoveParams; provider: string; retried: boolean; job: string | null } | null>(null);

  useEffect(() => {
    if (!open) {
      gen.current++;
      return;
    }
    const g = ++gen.current;
    // Opened for a job (the "Review" toast of a bring-back): show it directly.
    if (props.job) {
      setStep({ k: 'job', job: props.job });
      return;
    }
    setStep({ k: 'loading' });
    void (async () => {
      await stores.refresh(host);
      if (g !== gen.current) return;
      if (props.mode === 'bring_back') {
        const r = await app.conn(host)?.request('handoff.peers', {}).catch(() => null);
        if (g !== gen.current) return;
        setPeers(r?.peers ?? []);
        setStep(props.pane || props.box ? { k: 'target', box: props.box ?? null } : { k: 'pickbox' });
      } else setStep({ k: 'provider' });
    })();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [open, host]);

  const close = () => {
    gen.current++;
    onClose();
  };

  /** Start a move. `retried`: this is the one retry after signing in (no second prompt). */
  const move = async (params: MoveParams, provider: string, retried = false) => {
    const conn = app.conn(host);
    if (!conn) return;
    const g = gen.current;
    const interrupt = !!params.interrupt;
    setStep({ k: 'starting' });
    const body = { ...(props.pane ? { pane: props.pane } : {}), ...(props.mode === 'bring_back' && props.box && !props.pane ? { box: props.box } : {}), ...params };
    const cur = { params, provider, retried, job: null as string | null };
    last.current = cur;
    const signIn = async (p: string, methods: Parameters<typeof requestCloudAuth>[2]) => {
      if (cur.retried) return false;
      cur.retried = true;
      return requestCloudAuth(conn, p || provider, methods);
    };
    try {
      const { job } = await runWithCloudAuth(() => conn.request('cloud.move', body, { timeoutMs: 60_000 }), signIn);
      stores.trackJob(host, job);
      cur.job = job.id;
      if (g !== gen.current) return;
      setStep({ k: 'job', job: job.id });
    } catch (e) {
      if (g !== gen.current) return;
      if (!interrupt && isHandoffBusy(e)) return setStep({ k: 'busy', retry: () => void move({ ...params, interrupt: true }, provider, cur.retried) });
      const shared = !params.new_task ? sharedCheckout(e) : null;
      if (shared) return setStep({ k: 'shared', message: shared.message, retry: () => void move({ ...params, new_task: true }, provider, cur.retried) });
      if (e instanceof OutcomeUnknownError) {
        void stores.refresh(host);
        return setStep({ k: 'unknown' });
      }
      setStep({ k: 'error', message: errorMessage(e), retry: null });
    }
  };

  // The job failed because the sign-in no longer works (e.g. a revoked token): sign in, then
  // start the same move once more. A second failure of that kind stays a failure.
  const jobId = step.k === 'job' ? step.job : null;
  const job = jobId ? cloud.jobs.find((j) => j.id === jobId) : undefined;
  const authFail = job ? cloudJobNeedsAuth(job) : null;
  useEffect(() => {
    const cur = last.current;
    const conn = app.conn(host);
    if (!authFail || !job || !cur || cur.job !== job.id || cur.retried || !conn) return;
    cur.retried = true;
    const g = gen.current;
    const provider = authFail.provider ?? cur.provider;
    const methods = authFail.methods.length ? authFail.methods : (cloud.providers.find((p) => p.id === provider)?.methods ?? []);
    void requestCloudAuth(conn, provider, methods).then((ok) => {
      if (ok && g === gen.current) void move(cur.params, cur.provider, true);
    });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [authFail, job?.id]);

  const title = send ? t.cloud.title : t.cloud.bringBackTitle;
  return (
    <Sheet open={open} onClose={close} title={title}>
      {step.k === 'job' ? (
        <JobView host={host} jobId={step.job} onClose={close} />
      ) : (
        <Body step={step} send={send} host={host} providers={cloud.providers} boxes={cloud.boxes} taskKeys={taskKeys} peers={peers} setStep={setStep} move={move} onClose={close} />
      )}
    </Sheet>
  );
}

function Body({
  step,
  send,
  host,
  providers,
  boxes,
  taskKeys,
  peers,
  setStep,
  move,
  onClose,
}: {
  step: Exclude<Step, { k: 'job' }>;
  send: boolean;
  host: string;
  providers: CloudProvider[];
  boxes: CloudBox[];
  /** The source pane's task (id, handle, slug): its own box is the only existing one offered. */
  taskKeys: string[];
  peers: HandoffPeer[];
  setStep(s: Step): void;
  move(p: MoveParams, provider: string): Promise<void>;
  onClose(): void;
}) {
  const app = useApp();
  const [signingIn, setSigningIn] = useState(false);

  const chooseProvider = async (p: CloudProvider) => {
    if (p.auth.state !== 'ok') {
      const conn = app.conn(host);
      if (!conn) return;
      setSigningIn(true);
      const ok = await requestCloudAuth(conn, p.id, p.methods, p.label);
      setSigningIn(false);
      if (!ok) return;
    }
    setStep({ k: 'box', provider: p });
  };

  const live = useMemo(() => boxes.filter((b) => (b.ownership === 'attached' || b.ownership === 'idle') && b.state !== 'destroyed'), [boxes]);

  switch (step.k) {
    case 'loading':
    case 'starting':
      return <Working text={step.k === 'loading' ? t.loading : t.cloud.states['creating'] ?? ''} />;
    case 'provider':
      return (
        <div className="space-y-2">
          <div className="text-sm text-muted">{t.cloud.chooseProvider}</div>
          {providers.length === 0 && <Notice>{t.cloud.noProviders}</Notice>}
          {providers.map((p) => (
            <Choice
              key={p.id}
              icon={<Cloud className="size-5 text-muted" />}
              title={p.label}
              sub={p.auth.state === 'ok' ? t.cloud.signedIn(p.auth.account ?? '') : p.auth.state === 'invalid' ? t.cloud.invalid : t.cloud.signedOut}
              disabled={signingIn}
              onClick={() => void chooseProvider(p)}
            />
          ))}
          <div className="pt-1 text-xs text-muted">{t.cloud.sourceKept}</div>
        </div>
      );
    case 'box': {
      const mine = sendBoxChoices(boxes, step.provider.id, taskKeys);
      return (
        <div className="space-y-2">
          <div className="text-sm text-muted">{t.cloud.chooseBox}</div>
          <Choice icon={<Box className="size-5 text-muted" />} title={t.cloud.newBox} onClick={() => setStep({ k: 'confirm', provider: step.provider, box: null })} />
          {mine.map((b) => (
            <Choice key={b.box} icon={<Box className="size-5 text-muted" />} title={b.name} sub={`${t.cloud.existingBox} · ${b.state}`} onClick={() => setStep({ k: 'confirm', provider: step.provider, box: b.box })} />
          ))}
          <Button block variant="ghost" onClick={() => setStep({ k: 'provider' })}>
            {t.back}
          </Button>
        </div>
      );
    }
    case 'confirm':
      return (
        <div className="space-y-3">
          <Notice>{step.box ? `${step.provider.label} · ${step.box}` : `${step.provider.label} · ${t.cloud.newBox}`}</Notice>
          <div className="text-xs text-muted">{t.cloud.sourceKept}</div>
          <Button block size="lg" variant="primary" icon={<Cloud className="size-5" />} onClick={() => void move({ to: { kind: 'cloud', provider: step.provider.id, ...(step.box ? { box: step.box } : {}) } }, step.provider.id)}>
            {t.cloud.confirmSend(step.provider.label)}
          </Button>
          <Button block variant="ghost" onClick={() => setStep({ k: 'box', provider: step.provider })}>
            {t.back}
          </Button>
        </div>
      );
    case 'pickbox': {
      const withPanes = live.filter((b) => b.panes.length > 0);
      return (
        <div className="space-y-2">
          {withPanes.length === 0 && <Notice>{t.cloud.noBoxPanes}</Notice>}
          {withPanes.map((b) => (
            <Choice key={b.box} icon={<Box className="size-5 text-muted" />} title={b.name} sub={`${b.provider} · ${t.cloud.panes(b.panes.length)}`} onClick={() => setStep({ k: 'target', box: b.box })} />
          ))}
        </div>
      );
    }
    case 'target': {
      const prov = step.box?.split('/')[0] ?? '';
      const go = (to: CloudMoveTarget) => void move({ to, ...(step.box ? { box: step.box } : {}) }, prov);
      return (
        <div className="space-y-2">
          <div className="text-sm text-muted">{t.cloud.bringTo}</div>
          <Choice icon={<Server className="size-5 text-muted" />} title={t.cloud.thisHost} onClick={() => go({ kind: 'local' })} />
          {peers
            .filter((p) => !p.expired)
            .map((p) => (
              <Choice key={p.id} icon={<Server className="size-5 text-muted" />} title={p.name} onClick={() => go({ kind: 'peer', peer: p.id })} />
            ))}
        </div>
      );
    }
    case 'shared':
      return (
        <div className="space-y-3">
          <Notice tone="warn">{step.message || t.cloud.sharedCheckout}</Notice>
          <div className="text-xs text-muted">{t.cloud.newTaskHint}</div>
          <Button block size="lg" variant="primary" icon={<Cloud className="size-5" />} data-act="new-task-send" onClick={step.retry}>
            {t.cloud.newTaskSend}
          </Button>
          <Button block variant="ghost" onClick={onClose}>
            {t.cancel}
          </Button>
        </div>
      );
    case 'busy':
      return (
        <div className="space-y-3">
          <Notice tone="warn">{t.handoff.busy}</Notice>
          <Button block variant="danger" size="lg" icon={<OctagonX className="size-5" />} onClick={step.retry}>
            {t.handoff.interruptAndHandOff}
          </Button>
          <Button block variant="ghost" onClick={onClose}>
            {t.cancel}
          </Button>
        </div>
      );
    case 'unknown':
      return (
        <div className="space-y-3">
          <Notice tone="warn">{t.handoff.unknownSend}</Notice>
          <Button block variant="outline" onClick={onClose}>
            {t.close}
          </Button>
        </div>
      );
    case 'error':
      return (
        <div className="space-y-3">
          <div className="flex items-center gap-2 font-medium">
            <CircleAlert className="size-5 text-danger" />
            {t.handoff.failed}
          </div>
          <Notice tone="danger">{step.message}</Notice>
          <Button block variant="ghost" onClick={() => setStep(send ? { k: 'provider' } : { k: 'pickbox' })}>
            {t.back}
          </Button>
        </div>
      );
  }
}

function Choice({ icon, title, sub, onClick, disabled }: { icon: React.ReactNode; title: string; sub?: string; onClick(): void; disabled?: boolean }) {
  return (
    <button type="button" disabled={disabled} onClick={onClick} className="flex min-h-14 w-full items-center gap-3 rounded-xl border border-border px-3 text-left active:bg-surface-2 disabled:opacity-40">
      {icon}
      <div className="min-w-0 flex-1">
        <div className="truncate text-base font-medium">{title}</div>
        {sub && <div className="truncate text-xs text-muted">{sub}</div>}
      </div>
      <ArrowRight className="size-4 text-muted" />
    </button>
  );
}

/** A running or finished move: the state, a progress bar, and where the work ended up. */
function JobView({ host, jobId, onClose }: { host: string; jobId: string; onClose(): void }) {
  const app = useApp();
  const stores = useCloudStores();
  const job: CloudJob | undefined = useHostCloud(host).jobs.find((j) => j.id === jobId);
  const [err, setErr] = useState<string | null>(null);
  const [cancelling, setCancelling] = useState(false);
  const final = job ? cloudJobFinal(job) : false;

  useEffect(() => stores.watch(host, jobId), [stores, host, jobId]);
  useEffect(() => {
    if (final) return;
    const id = setInterval(() => void stores.refresh(host), JOB_POLL_MS);
    return () => clearInterval(id);
  }, [final, stores, host]);

  if (!job) return <Working text={t.cloud.states['queued'] ?? ''} />;
  const pct = cloudJobPct(job);

  const cancel = async () => {
    const conn = app.conn(host);
    if (!conn) return;
    setCancelling(true);
    try {
      await conn.request('cloud.cancel', { id: job.id });
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setCancelling(false);
    }
  };

  if (job.state === 'failed')
    return (
      <div className="space-y-3">
        <div className="flex items-center gap-2 font-medium">
          <CircleAlert className="size-5 text-danger" />
          {t.handoff.failed}
        </div>
        <Notice tone="danger">{cloudJobError(job) ?? t.unknownError}</Notice>
        <Button block variant="ghost" onClick={onClose}>
          {t.close}
        </Button>
      </div>
    );
  if (job.state === 'awaiting_confirmation' && job.result?.review)
    return <ReviewView host={host} job={job} review={job.result.review} onClose={onClose} />;
  if (job.state === 'cancelled')
    return (
      <div className="space-y-3">
        <Notice>{t.cloud.states['cancelled'] ?? ''}</Notice>
        <Button block variant="ghost" onClick={onClose}>
          {t.close}
        </Button>
      </div>
    );
  if (job.state === 'done') {
    const pane = job.result?.pane;
    return (
      <div className="space-y-3">
        <div className="flex items-center gap-2 text-lg font-medium">
          <CheckCircle2 className="size-6 text-ok" />
          {t.cloud.jobDone}
        </div>
        {pane && (
          <Button
            block
            size="lg"
            variant="primary"
            icon={<Check className="size-5" />}
            onClick={() => {
              onClose();
              navigate({ name: 'pane', host, pane, view: 'term' });
            }}
          >
            {t.cloud.open}
          </Button>
        )}
        <Button block variant="ghost" onClick={onClose}>
          {t.done}
        </Button>
      </div>
    );
  }
  return (
    <div className="space-y-3">
      <div className="text-base">{t.cloud.states[job.state] ?? job.state}</div>
      <div className="h-2 overflow-hidden rounded-full bg-surface-2" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct ?? undefined} aria-busy={pct === null}>
        <div className={pct === null ? 'h-full w-1/3 animate-pulse bg-accent/60' : 'h-full bg-accent transition-[width]'} style={pct === null ? undefined : { width: `${pct}%` }} />
      </div>
      <div className="text-xs text-muted">{t.handoff.background}</div>
      {err && <Notice tone="danger">{err}</Notice>}
      <div className="flex gap-2">
        <Button block variant="outline" busy={cancelling} onClick={() => void cancel()}>
          {t.cancel}
        </Button>
        <Button block variant="primary" onClick={onClose}>
          {t.close}
        </Button>
      </div>
    </div>
  );
}

/**
 * A bring-back to this host waits here: what comes back (files with line counts, new and skipped
 * files, a conversation) and the host's warning. Apply brings the work and resumes the agent;
 * Code only brings the files; Cancel stops the move and leaves the sandbox as it is.
 */
function ReviewView({ host, job, review, onClose }: { host: string; job: CloudJob; review: CloudReview; onClose(): void }) {
  const app = useApp();
  const [busy, setBusy] = useState<'apply' | 'code' | 'cancel' | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const act = async (what: 'apply' | 'code' | 'cancel') => {
    const conn = app.conn(host);
    if (!conn) return;
    setBusy(what);
    setErr(null);
    try {
      if (what === 'cancel') await conn.request('cloud.cancel', { id: job.id });
      else await conn.request('cloud.confirm', { id: job.id, ...(what === 'code' ? { code_only: true } : {}) });
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(null);
    }
  };
  const moreFiles = review.files_total - review.files.length;
  const moreUntracked = review.untracked_total - review.untracked.length;
  return (
    <div className="space-y-3" data-cloud-review={job.id}>
      <div className="text-base font-medium">{t.cloud.reviewTitle}</div>
      {review.warning && <Notice tone="warn">{review.warning}</Notice>}
      <div className="text-sm text-muted">
        {t.cloud.reviewSummary(review.files_total, review.added, review.removed)}
        {review.branch ? ` · ${review.branch}` : ''}
      </div>
      {review.files.length > 0 && (
        <ul aria-label={t.cloud.reviewFiles} className="max-h-48 overflow-y-auto rounded-xl border border-border font-mono text-xs">
          {review.files.map((f) => (
            <li key={f.path} className="flex items-center gap-2 border-b border-border px-2.5 py-1 last:border-b-0">
              <span className="min-w-0 flex-1 truncate" title={f.path}>
                {f.path}
              </span>
              <span className="shrink-0 text-ok">+{f.added}</span>
              <span className="shrink-0 text-danger">−{f.removed}</span>
            </li>
          ))}
          {moreFiles > 0 && <li className="px-2.5 py-1 text-muted">{t.cloud.reviewMore(moreFiles)}</li>}
        </ul>
      )}
      {review.untracked.length > 0 && (
        <div className="space-y-1">
          <div className="text-xs font-medium text-muted">{t.cloud.reviewUntracked}</div>
          <div className="max-h-24 overflow-y-auto font-mono text-xs">
            {review.untracked.map((p) => (
              <div key={p} className="truncate" title={p}>
                {p}
              </div>
            ))}
            {moreUntracked > 0 && <div className="text-muted">{t.cloud.reviewMore(moreUntracked)}</div>}
          </div>
        </div>
      )}
      {review.skipped.length > 0 && (
        <div className="space-y-1">
          <div className="text-xs font-medium text-muted">{t.cloud.reviewSkipped}</div>
          <div className="max-h-24 overflow-y-auto font-mono text-xs">
            {review.skipped.map((s) => (
              <div key={s.path} className="truncate" title={`${s.path}: ${s.reason}`}>
                {s.path} <span className="text-muted">({s.reason})</span>
              </div>
            ))}
          </div>
        </div>
      )}
      <div className="text-xs text-muted">{review.transcript ? t.cloud.reviewWithConversation : t.cloud.reviewNoConversation}</div>
      {err && <Notice tone="danger">{err}</Notice>}
      <div className="flex flex-col gap-2">
        <Button block size="lg" variant="primary" busy={busy === 'apply'} disabled={busy !== null} data-act="cloud-apply" onClick={() => void act('apply')}>
          {t.cloud.reviewApply}
        </Button>
        <Button block variant="outline" busy={busy === 'code'} disabled={busy !== null} data-act="cloud-code-only" onClick={() => void act('code')}>
          {t.cloud.reviewCodeOnly}
        </Button>
        <Button block variant="ghost" busy={busy === 'cancel'} disabled={busy !== null} data-act="cloud-review-cancel" onClick={() => void act('cancel')}>
          {t.cloud.reviewCancel}
        </Button>
      </div>
      <Button block variant="ghost" onClick={onClose}>
        {t.close}
      </Button>
    </div>
  );
}

function Working({ text }: { text: string }) {
  return (
    <div className="flex items-center gap-3 py-6 text-base">
      <Spinner />
      {text}
    </div>
  );
}
