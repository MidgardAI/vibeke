// Hand off a pane (spec 16 §15.2): choose where its work goes, and the source host sends it
// there itself (`handoff.send`), gateway to gateway. The app only starts the job and shows its
// progress (`handoff.job` events, `handoff.jobs` as a fallback poll), so the sheet can close at
// any point and the transfer goes on. The receiving host decides where the work lands.
//
// Destinations are the source host's peers plus the user's other full-access hosts. Choosing
// one of those that is not a peer yet pairs the two hosts first (`peer.invite` on the
// destination, `peer.redeem` on the source; the app holds full access to both).
//
// Rules: every call carries an op_id (core rpc); nothing mutating is retried automatically.

import { useEffect, useRef, useState } from 'react';
import { ArrowRight, CheckCircle2, CircleAlert, Link2, OctagonX, Server, Users } from 'lucide-react';
import { OutcomeUnknownError, RpcError, isHandoffBusy, type HandoffJob, type HandoffPeer, type PeerInfo } from '@vibeke/core';
import { useHandoffStores, useHostIncoming, useJobs } from '../app/handoff-stores';
import { useAllHosts, useApp, useNow } from '../app/hooks';
import { Button, Dot, Notice, Sheet, Spinner } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { byteSize, whenText } from '../lib/format';
import { findIncomingForJob, jobFinal, jobView, mergePeers, planSend, sendDestinations, type SendDest } from '../lib/handoff-send';
import { importedTarget } from '../lib/incoming';
import type { PaneRow } from '../lib/tree';
import { navigate } from '../router';

type Step =
  | { k: 'loading' }
  | { k: 'choose'; dests: SendDest[]; warning: string | null }
  | { k: 'pairing'; dest: SendDest }
  | { k: 'starting'; dest: SendDest }
  | { k: 'busy'; dest: SendDest; peer: string }
  | { k: 'job'; dest: SendDest; peer: string; job: string; startedAt: number }
  | { k: 'unknown'; dest: SendDest }
  | { k: 'error'; dest: SendDest | null; message: string; retry: (() => void) | null };

/** Long enough for a relay round trip to the other host and its pairing. */
const PAIR_TIMEOUT_MS = 90_000;
/** Poll `handoff.jobs` while a job runs, in case events are not reaching us. */
const JOB_POLL_MS = 3000;

/** The job failed because the agent is working (the export refused without `interrupt`). */
const busyJob = (j: HandoffJob): boolean => {
  const e = j.error;
  if (!e) return false;
  if (typeof e === 'string') return /\bbusy\b|working/i.test(e);
  return e.kind === 'busy' || (e.kind === 'conflict' && /working/i.test(e.message ?? ''));
};

export function HandoffSheet({ row, open, onClose }: { row: PaneRow; open: boolean; onClose(): void }) {
  const app = useApp();
  const hosts = useAllHosts();
  const stores = useHandoffStores();
  const [step, setStep] = useState<Step>({ k: 'loading' });
  const gen = useRef(0);
  const source = () => app.conn(row.host);
  const nowS = () => Math.floor(app.platform.clock.now() / 1000);
  const hostsRef = useRef(hosts);
  hostsRef.current = hosts;

  const load = async () => {
    const g = ++gen.current;
    setStep({ k: 'loading' });
    const src = source();
    if (!src) return;
    // `handoff.peers` is what the source can send to; `peer.list` adds each peer's host id.
    const [hp, pl] = await Promise.allSettled([src.request('handoff.peers', {}), src.request('peer.list', {})]);
    if (g !== gen.current) return;
    const handoffPeers: HandoffPeer[] | null = hp.status === 'fulfilled' ? (hp.value.peers ?? []) : null;
    const peerList: PeerInfo[] | null = pl.status === 'fulfilled' ? (pl.value.peers ?? []) : null;
    const warning = handoffPeers === null && peerList === null ? errorMessage(hp.status === 'rejected' ? hp.reason : null) : null;
    const dests = sendDestinations(row.host, mergePeers(handoffPeers, peerList), hostsRef.current, nowS());
    setStep({ k: 'choose', dests, warning });
  };

  useEffect(() => {
    if (open) void load();
    else gen.current++;
  }, [open, row.host, row.pane.id]);

  const close = () => {
    gen.current++;
    onClose();
    // Let the sheet animate away before resetting; a running job goes on (and toasts when done).
    setTimeout(() => setStep({ k: 'loading' }), 0);
  };

  const send = async (dest: SendDest, peer: string, interrupt: boolean) => {
    const src = source();
    if (!src) return;
    const g = gen.current;
    setStep({ k: 'starting', dest });
    try {
      const r = await src.request('handoff.send', { pane: row.pane.id, peer, ...(interrupt ? { interrupt: true } : {}) }, { timeoutMs: 60_000 });
      // Tracked even when the sheet closed meanwhile: its outcome then arrives as a toast.
      stores.trackJob(row.host, r.job);
      if (g !== gen.current) return;
      setStep({ k: 'job', dest, peer, job: r.job.id, startedAt: r.job.created_at_ms ?? app.platform.clock.now() });
    } catch (e) {
      if (g !== gen.current) return;
      if (!interrupt && isHandoffBusy(e)) return setStep({ k: 'busy', dest, peer });
      if (e instanceof OutcomeUnknownError) {
        // Never re-sent by us: the source may have started it. Its jobs show what happened.
        void stores.refreshJobs(row.host);
        return setStep({ k: 'unknown', dest });
      }
      setStep({ k: 'error', dest, message: errorMessage(e), retry: e instanceof RpcError ? () => void send(dest, peer, interrupt) : null });
    }
  };

  /** Pair the source with one of the user's hosts, then send. */
  const pairThenSend = async (dest: SendDest, hostId: string) => {
    const src = source();
    const dst = app.conn(hostId);
    if (!src || !dst) return;
    const g = gen.current;
    setStep({ k: 'pairing', dest });
    let pid: string | null = null;
    try {
      const inv = await dst.request('peer.invite', {});
      pid = inv.pid;
      const r = await src.request('peer.redeem', { link: inv.link }, { timeoutMs: PAIR_TIMEOUT_MS });
      pid = null;
      if (g !== gen.current) return;
      await send({ ...dest, peer: r.peer.id, name: r.peer.name || dest.name }, r.peer.id, false);
    } catch (e) {
      // An unused invitation would sit there until it expires: cancel it.
      if (pid) void dst.request('share.revoke', { id: pid }).catch(() => {});
      if (g !== gen.current) return;
      setStep({ k: 'error', dest, message: `${t.handoff.pairFailed} ${errorMessage(e)}`, retry: () => void pairThenSend(dest, hostId) });
    }
  };

  const pick = (d: SendDest) => {
    const plan = planSend(d);
    if (plan.k === 'send') void send(d, plan.peer, false);
    else if (plan.k === 'pair') void pairThenSend(d, plan.hostId);
  };

  return (
    <Sheet open={open} onClose={close} title={t.handoff.title}>
      {step.k === 'job' ? (
        <JobView step={step} sourceHost={row.host} sourceName={row.hostName} onClose={close} onRetry={(interrupt) => void send(step.dest, step.peer, interrupt)} onBack={() => void load()} />
      ) : (
        <Body step={step} onPick={pick} onInterrupt={(d, peer) => void send(d, peer, true)} onBack={() => void load()} onClose={close} />
      )}
    </Sheet>
  );
}

function Body({ step, onPick, onInterrupt, onBack, onClose }: { step: Exclude<Step, { k: 'job' }>; onPick(d: SendDest): void; onInterrupt(d: SendDest, peer: string): void; onBack(): void; onClose(): void }) {
  const now = useNow(60_000);
  switch (step.k) {
    case 'loading':
      return <Working text={t.loading} />;
    case 'choose':
      return (
        <div className="space-y-2">
          <div className="text-sm text-muted">{t.handoff.chooseDest}</div>
          {step.warning && <Notice tone="warn">{step.warning}</Notice>}
          {step.dests.length === 0 && <Notice>{t.handoff.noDest}</Notice>}
          {step.dests.map((d) => {
            const plan = planSend(d);
            const usable = plan.k !== 'unavailable';
            const sub = [
              d.peer === null ? t.handoff.pairsFirst : d.owner === 'self' ? t.handoff.ownHost : t.handoff.teammate,
              d.expired
                ? t.handoff.expired
                : d.expiresAt !== null
                  ? t.handoff.until(whenText(d.expiresAt * 1000, now))
                  : null,
              plan.k === 'unavailable' && plan.reason === 'offline' ? t.handoff.offline : null,
            ].filter(Boolean);
            return (
              <button
                key={d.key}
                type="button"
                disabled={!usable}
                onClick={() => onPick(d)}
                className="flex min-h-14 w-full items-center gap-3 rounded-xl border border-border px-3 text-left active:bg-surface-2 disabled:opacity-40"
              >
                {d.owner === 'teammate' ? <Users className="size-5 text-muted" /> : d.peer === null ? <Link2 className="size-5 text-muted" /> : <Server className="size-5 text-muted" />}
                <div className="min-w-0 flex-1">
                  <div className="truncate text-base font-medium">{d.name}</div>
                  <div className="text-xs text-muted">{sub.join(' · ')}</div>
                </div>
                {d.peer === null && <Dot tone={d.online ? 'ok' : 'muted'} />}
                <ArrowRight className="size-4 text-muted" />
              </button>
            );
          })}
          <div className="pt-1 text-xs text-muted">{t.handoff.sourceKept}</div>
        </div>
      );
    case 'pairing':
      return <Working text={t.handoff.pairing(step.dest.name)} />;
    case 'starting':
      return <Working text={t.handoff.starting(step.dest.name)} />;
    case 'busy':
      return (
        <div className="space-y-3">
          <Notice tone="warn">{t.handoff.busy}</Notice>
          <Button block variant="danger" size="lg" icon={<OctagonX className="size-5" />} onClick={() => onInterrupt(step.dest, step.peer)}>
            {t.handoff.interruptAndHandOff}
          </Button>
          <Button block variant="ghost" onClick={onBack}>
            {t.back}
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
          {step.retry && (
            <Button block variant="primary" onClick={step.retry}>
              {t.handoff.tryAgain}
            </Button>
          )}
          <Button block variant="ghost" onClick={onBack}>
            {t.back}
          </Button>
        </div>
      );
  }
}

/** A running or finished job: progress, then what the destination made of it. */
function JobView({
  step,
  sourceHost,
  sourceName,
  onClose,
  onRetry,
  onBack,
}: {
  step: Extract<Step, { k: 'job' }>;
  sourceHost: string;
  sourceName: string;
  onClose(): void;
  onRetry(interrupt: boolean): void;
  onBack(): void;
}) {
  const app = useApp();
  const stores = useHandoffStores();
  const jobs = useJobs();
  const job = jobs.get(sourceHost)?.find((j) => j.id === step.job);
  const dest = step.dest;
  const destIncoming = useHostIncoming(dest.hostId ?? '');
  const [cancelling, setCancelling] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const final = job ? jobFinal(job) : false;

  // The sheet reports this job itself (no toast while it is shown).
  useEffect(() => stores.watch(sourceHost, step.job), [stores, sourceHost, step.job]);

  // Fallback when events do not arrive (older shells, a reconnect): poll the source's jobs.
  useEffect(() => {
    if (final) return;
    const id = setInterval(() => void stores.refreshJobs(sourceHost), JOB_POLL_MS);
    return () => clearInterval(id);
  }, [final, stores, sourceHost]);

  if (!job) return <Working text={t.handoff.starting(dest.name)} />;
  const v = jobView(job);
  // On one of the user's own hosts the imported pane can be opened directly.
  const record = dest.hostId && v.phase === 'imported' ? findIncomingForJob(destIncoming.list, sourceName, step.startedAt, null) : null;
  const target = record ? importedTarget(record) : null;

  const cancel = async () => {
    const conn = app.conn(sourceHost);
    if (!conn) return;
    setCancelling(true);
    setErr(null);
    try {
      await conn.request('handoff.cancel', { id: job.id });
      void stores.refreshJobs(sourceHost);
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setCancelling(false);
    }
  };

  const open = () => {
    if (!dest.hostId) return;
    onClose();
    if (target && 'pane' in target) navigate({ name: 'pane', host: dest.hostId, pane: target.pane, view: 'term' });
    else navigate({ name: 'handoffs', host: dest.hostId, id: record?.id ?? null });
  };

  switch (v.phase) {
    case 'queued':
    case 'exporting':
    case 'sending':
      return (
        <div className="space-y-3">
          <div className="text-base">{v.phase === 'sending' ? t.handoff.sending(dest.name) : v.phase === 'exporting' ? t.handoff.exporting : t.handoff.queued}</div>
          <Progress pct={v.phase === 'sending' ? v.pct : null} />
          {v.phase === 'sending' && v.total > 0 && (
            <div className="text-xs tabular-nums text-muted">
              {byteSize(v.sent)} / {byteSize(v.total)} · {v.pct ?? 0}%
            </div>
          )}
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
    case 'failed': {
      const busy = busyJob(job);
      return (
        <div className="space-y-3">
          <div className="flex items-center gap-2 font-medium">
            <CircleAlert className="size-5 text-danger" />
            {t.handoff.failed}
          </div>
          <Notice tone={busy ? 'warn' : 'danger'}>{busy ? t.handoff.busy : (v.error ?? t.unknownError)}</Notice>
          {busy ? (
            <Button block variant="danger" icon={<OctagonX className="size-5" />} onClick={() => onRetry(true)}>
              {t.handoff.interruptAndHandOff}
            </Button>
          ) : (
            <Button block variant="primary" onClick={() => onRetry(false)}>
              {t.handoff.tryAgain}
            </Button>
          )}
          <Button block variant="ghost" onClick={onBack}>
            {t.back}
          </Button>
        </div>
      );
    }
    case 'cancelled':
      return (
        <div className="space-y-3">
          <Notice>{t.handoff.cancelled}</Notice>
          <Button block variant="ghost" onClick={onBack}>
            {t.back}
          </Button>
        </div>
      );
    default:
      // Delivered: pending on the recipient, being set up, imported, failed or declined there.
      return (
        <div className="space-y-3">
          <div className="flex items-center gap-2 text-lg font-medium">
            <CheckCircle2 className="size-6 text-ok" />
            {v.phase === 'imported' ? t.handoff.success(dest.name) : t.handoff.delivered(dest.name)}
          </div>
          {v.phase === 'pending' && <Notice>{t.handoff.pending}</Notice>}
          {v.phase === 'importing' && <Notice>{t.handoff.importing}</Notice>}
          {v.phase === 'import_failed' && <Notice tone="warn">{t.handoff.importFailedThere}</Notice>}
          {v.phase === 'declined' && <Notice tone="warn">{t.handoff.declined(dest.name)}</Notice>}
          {v.phase === 'delivered' && <Notice>{t.handoff.deliveredNote}</Notice>}
          {dest.hostId && (v.phase === 'imported' || v.phase === 'pending' || v.phase === 'import_failed') && (
            <Button block size="lg" variant={v.phase === 'imported' ? 'primary' : 'outline'} onClick={open}>
              {v.phase === 'imported' ? t.handoff.openOn(dest.name) : t.handoff.acceptOn(dest.name)}
            </Button>
          )}
          <Button block variant="ghost" onClick={onClose}>
            {t.done}
          </Button>
        </div>
      );
  }
}

function Progress({ pct }: { pct: number | null }) {
  if (pct === null)
    return (
      <div className="h-2 overflow-hidden rounded-full bg-surface-2" role="progressbar" aria-busy>
        <div className="h-full w-1/3 animate-pulse bg-accent/60" />
      </div>
    );
  return (
    <div className="h-2 overflow-hidden rounded-full bg-surface-2" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct}>
      <div className="h-full bg-accent transition-[width]" style={{ width: `${pct}%` }} />
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
