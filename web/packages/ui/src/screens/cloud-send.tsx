// Move work to and from a cloud sandbox (spec 17 §7). One sheet, two modes:
//
// - send: pick a provider (sign in when it needs it), pick a new or an existing sandbox,
//   confirm, then follow the `cloud.move` job.
// - bring back: pick where the work goes (this host, or a peer from `handoff.peers`), then
//   follow the job.
//
// The host runs the job, so the sheet can close at any point and the move goes on; a toast
// reports the end. Nothing mutating is retried by the app, except the one retry after sign-in.

import { useEffect, useMemo, useRef, useState } from 'react';
import { ArrowRight, Check, CheckCircle2, CircleAlert, Cloud, OctagonX, Server, Box } from 'lucide-react';
import { OutcomeUnknownError, isHandoffBusy, type CloudBox, type CloudJob, type CloudMoveTarget, type CloudProvider, type HandoffPeer } from '@vibeke/core';
import { useCloudStores, useHostCloud } from '../app/cloud-stores';
import { useApp } from '../app/hooks';
import { requestCloudAuth, withCloudAuth } from '../components/cloud-auth';
import { Button, Notice, Sheet, Spinner } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { cloudJobError, cloudJobFinal, cloudJobPct } from '../lib/cloud';
import { navigate } from '../router';

export type CloudSheetProps =
  | { mode: 'send'; host: string; pane: string; open: boolean; onClose(): void }
  | { mode: 'bring_back'; host: string; pane?: string; box?: string; open: boolean; onClose(): void };

type Step =
  | { k: 'loading' }
  | { k: 'provider' }
  | { k: 'box'; provider: CloudProvider }
  | { k: 'confirm'; provider: CloudProvider; box: string | null }
  | { k: 'pickbox' }
  | { k: 'target'; box: string | null }
  | { k: 'starting' }
  | { k: 'busy'; retry: () => void }
  | { k: 'job'; job: string }
  | { k: 'unknown' }
  | { k: 'error'; message: string; retry: (() => void) | null };

const JOB_POLL_MS = 3000;

export function CloudSheet(props: CloudSheetProps) {
  const { host, open, onClose } = props;
  const app = useApp();
  const stores = useCloudStores();
  const cloud = useHostCloud(host);
  const [step, setStep] = useState<Step>({ k: 'loading' });
  const [peers, setPeers] = useState<HandoffPeer[]>([]);
  const gen = useRef(0);
  const send = props.mode === 'send';

  useEffect(() => {
    if (!open) {
      gen.current++;
      return;
    }
    const g = ++gen.current;
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

  const move = async (params: { to: CloudMoveTarget; box?: string; interrupt?: boolean; source_after?: 'keep' | 'suspend' | 'destroy' }, provider: string) => {
    const conn = app.conn(host);
    if (!conn) return;
    const g = gen.current;
    const interrupt = !!params.interrupt;
    setStep({ k: 'starting' });
    const body = { ...(props.pane ? { pane: props.pane } : {}), ...(props.mode === 'bring_back' && props.box && !props.pane ? { box: props.box } : {}), ...params };
    try {
      const { job } = await withCloudAuth(conn, provider, () => conn.request('cloud.move', body, { timeoutMs: 60_000 }));
      stores.trackJob(host, job);
      if (g !== gen.current) return;
      setStep({ k: 'job', job: job.id });
    } catch (e) {
      if (g !== gen.current) return;
      if (!interrupt && isHandoffBusy(e)) return setStep({ k: 'busy', retry: () => void move({ ...params, interrupt: true }, provider) });
      if (e instanceof OutcomeUnknownError) {
        void stores.refresh(host);
        return setStep({ k: 'unknown' });
      }
      setStep({ k: 'error', message: errorMessage(e), retry: null });
    }
  };

  const title = send ? t.cloud.title : t.cloud.bringBackTitle;
  return (
    <Sheet open={open} onClose={close} title={title}>
      {step.k === 'job' ? (
        <JobView host={host} jobId={step.job} onClose={close} />
      ) : (
        <Body step={step} send={send} host={host} providers={cloud.providers} boxes={cloud.boxes} peers={peers} setStep={setStep} move={move} onClose={close} />
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
  peers: HandoffPeer[];
  setStep(s: Step): void;
  move(p: { to: CloudMoveTarget; box?: string; interrupt?: boolean; source_after?: 'keep' | 'suspend' | 'destroy' }, provider: string): Promise<void>;
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
      const mine = live.filter((b) => b.provider === step.provider.id);
      return (
        <div className="space-y-2">
          <div className="text-sm text-muted">{t.cloud.chooseBox}</div>
          <Choice icon={<Box className="size-5 text-muted" />} title={t.cloud.newBox} onClick={() => setStep({ k: 'confirm', provider: step.provider, box: null })} />
          {mine.map((b) => (
            <Choice key={b.box} icon={<Box className="size-5 text-muted" />} title={b.task || b.name} sub={`${t.cloud.existingBox} · ${b.state}`} onClick={() => setStep({ k: 'confirm', provider: step.provider, box: b.box })} />
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
            <Choice key={b.box} icon={<Box className="size-5 text-muted" />} title={b.task || b.name} sub={`${b.provider} · ${t.cloud.panes(b.panes.length)}`} onClick={() => setStep({ k: 'target', box: b.box })} />
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

function Working({ text }: { text: string }) {
  return (
    <div className="flex items-center gap-3 py-6 text-base">
      <Spinner />
      {text}
    </div>
  );
}
