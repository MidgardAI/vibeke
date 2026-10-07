// Handoff (spec 16 §15.2): export the agent's work on this host, show what will travel, carry the
// bundle to another host in 2 MiB chunks, and deliver it there as an incoming handoff. The app is
// the courier; the receiving host decides where the work lands (accepting it, or importing it
// automatically at a remembered place).
//
// Rules: every call carries an op_id (core rpc); nothing mutating is retried automatically; a lost
// `handoff.finish` result is shown as "unknown, check the destination", never re-sent by us.

import { useEffect, useRef, useState, type ReactNode } from 'react';
import { ArrowRight, CheckCircle2, CircleAlert, KeyRound, OctagonX, Send, Server } from 'lucide-react';
import {
  HandoffCancelled,
  NotConnectedError,
  OutcomeUnknownError,
  RpcError,
  discardHandoff,
  exportHandoff,
  handoffSummary,
  hostKind,
  transferHandoff,
  type ExportedHandoff,
  type HandoffFinishResult,
  type HostState,
} from '@vibeke/core';
import { useAllHosts, useApp } from '../app/hooks';
import { Button, Dot, Notice, Sheet, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { byteSize } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import type { PaneRow } from '../lib/tree';
import { navigate } from '../router';

interface Dest {
  hostId: string;
  name: string;
  /** A teammate's host reached through a handoff invitation (no panes to open there). */
  invite: boolean;
}

type Step =
  | { k: 'choose' }
  | { k: 'exporting'; dest: Dest }
  | { k: 'busy'; dest: Dest }
  | { k: 'confirm'; dest: Dest; exp: ExportedHandoff }
  | { k: 'sending'; dest: Dest; exp: ExportedHandoff; sent: number; total: number }
  | { k: 'finishing'; dest: Dest; exp: ExportedHandoff; destId: string }
  | { k: 'done'; dest: Dest; result: HandoffFinishResult }
  | { k: 'unknown'; dest: Dest }
  | { k: 'error'; dest: Dest; message: string; retry: (() => void) | null };

const hostName = (h: HostState) => h.info?.host_name ?? h.record.name;

/** Paired hosts that may receive a handoff: own full-scope hosts, or handoff invitations. */
export function handoffDestinations(hosts: readonly HostState[], sourceHostId: string): HostState[] {
  return hosts.filter((h) => {
    if (h.record.host_id === sourceHostId) return false;
    const kind = hostKind(h.record);
    if (kind === 'handoff') return h.status !== 'expired';
    return kind === 'device' && (h.info?.scope ?? h.record.scope) === 'full';
  });
}

export function HandoffSheet({ row, open, onClose }: { row: PaneRow; open: boolean; onClose(): void }) {
  const app = useApp();
  const hosts = useAllHosts();
  const [step, setStep] = useState<Step>({ k: 'choose' });
  const cancel = useRef({ cancelled: false });
  // The source export to clean up if the user walks away before sending.
  const pendingExport = useRef<string | null>(null);
  const source = () => app.conn(row.host);
  const dests = handoffDestinations(hosts, row.host);

  const reset = () => {
    cancel.current = { cancelled: false };
    setStep({ k: 'choose' });
  };

  const dropExport = () => {
    const id = pendingExport.current;
    pendingExport.current = null;
    if (id) void discardHandoff(source() ?? null, id, null, null);
  };

  const close = () => {
    if (step.k === 'sending') cancel.current.cancelled = true;
    else dropExport();
    onClose();
    // Let the sheet animate away before resetting.
    setTimeout(reset, 0);
  };

  useEffect(() => () => dropExport(), []);

  const doExport = async (dest: Dest, interrupt: boolean) => {
    const conn = source();
    if (!conn) return;
    setStep({ k: 'exporting', dest });
    try {
      // A working agent answers `busy`: offer "Interrupt and hand off" (export again with interrupt).
      const r = await exportHandoff(conn, row.pane.id, interrupt);
      if (r.k === 'busy') return setStep({ k: 'busy', dest });
      pendingExport.current = r.exported.id;
      setStep({ k: 'confirm', dest, exp: r.exported });
    } catch (e) {
      setStep({ k: 'error', dest, message: errorMessage(e), retry: () => void doExport(dest, interrupt) });
    }
  };

  const doSend = async (dest: Dest, exp: ExportedHandoff) => {
    const src = source();
    const dst = app.conn(dest.hostId);
    if (!src || !dst) return;
    if (dst.getSnapshot().status !== 'online') return setStep({ k: 'error', dest, message: t.handoff.destOffline, retry: () => void doSend(dest, exp) });
    cancel.current = { cancelled: false };
    setStep({ k: 'sending', dest, exp, sent: 0, total: exp.size });
    let destId: string;
    try {
      destId = await transferHandoff({
        source: src,
        dest: dst,
        exported: exp,
        cancel: cancel.current,
        onProgress: (sent, total) => setStep((s) => (s.k === 'sending' ? { ...s, sent, total } : s)),
      });
    } catch (e) {
      if (e instanceof HandoffCancelled) {
        pendingExport.current = null; // discarded by the courier
        app.toast(t.handoff.cancelled);
        return;
      }
      // The source export is kept: "Try again" re-sends without exporting anew.
      setStep({ k: 'error', dest, message: errorMessage(e), retry: () => void doSend(dest, exp) });
      return;
    }
    await doFinish(dest, exp, destId);
  };

  const doFinish = async (dest: Dest, exp: ExportedHandoff, destId: string) => {
    const dst = app.conn(dest.hostId);
    if (!dst) return;
    setStep({ k: 'finishing', dest, exp, destId });
    try {
      const result = await dst.request('handoff.finish', { id: destId }, { timeoutMs: 300_000 });
      // The source copy is no longer needed.
      const id = pendingExport.current;
      pendingExport.current = null;
      if (id) void discardHandoff(source() ?? null, id, null, null);
      app.haptic(result.state === 'failed' || result.result?.agent_error ? 'warning' : 'success');
      if (!dest.invite) void dst.refresh().catch(() => {});
      setStep({ k: 'done', dest, result });
    } catch (e) {
      if (e instanceof OutcomeUnknownError) {
        // Never retried: the destination may have set it up already.
        app.haptic('warning');
        return setStep({ k: 'unknown', dest });
      }
      // A known failure (or not sent at all): the user may try again.
      const known = e instanceof RpcError || e instanceof NotConnectedError;
      setStep({ k: 'error', dest, message: errorMessage(e), retry: known ? () => void doFinish(dest, exp, destId) : null });
    }
  };

  return (
    <Sheet open={open} onClose={close} title={t.handoff.title}>
      <Body step={step} dests={dests} onPick={(d) => void doExport(d, false)} onInterrupt={(d) => void doExport(d, true)} onSend={(d, x) => void doSend(d, x)} onCancel={() => (cancel.current.cancelled = true)} onClose={close} onBack={() => (dropExport(), reset())} />
    </Sheet>
  );
}

function Body({
  step,
  dests,
  onPick,
  onInterrupt,
  onSend,
  onCancel,
  onClose,
  onBack,
}: {
  step: Step;
  dests: HostState[];
  onPick(d: Dest): void;
  onInterrupt(d: Dest): void;
  onSend(d: Dest, exp: ExportedHandoff): void;
  onCancel(): void;
  onClose(): void;
  onBack(): void;
}) {
  switch (step.k) {
    case 'choose':
      return (
        <div className="space-y-2">
          <div className="text-sm text-muted">{t.handoff.chooseDest}</div>
          {dests.length === 0 && <Notice>{t.handoff.noDest}</Notice>}
          {dests.map((h) => {
            const invite = hostKind(h.record) === 'handoff';
            const online = h.status === 'online';
            return (
              <button
                key={h.record.host_id}
                type="button"
                disabled={!online}
                onClick={() => onPick({ hostId: h.record.host_id, name: hostName(h), invite })}
                className="flex min-h-14 w-full items-center gap-3 rounded-xl border border-border px-3 text-left active:bg-surface-2 disabled:opacity-40"
              >
                <Server className="size-5 text-muted" />
                <div className="min-w-0 flex-1">
                  <div className="truncate text-base font-medium">{hostName(h)}</div>
                  <div className="text-xs text-muted">{[invite ? t.handoff.viaInvite : t.handoff.ownHost, online ? null : t.handoff.offline].filter(Boolean).join(' · ')}</div>
                </div>
                <Dot tone={online ? 'ok' : 'muted'} />
                <ArrowRight className="size-4 text-muted" />
              </button>
            );
          })}
          <div className="pt-1 text-xs text-muted">{t.handoff.sourceKept}</div>
        </div>
      );
    case 'exporting':
      return <Working text={t.handoff.exporting} />;
    case 'busy':
      return (
        <div className="space-y-3">
          <Notice tone="warn">{t.handoff.busy}</Notice>
          <Button block variant="danger" size="lg" icon={<OctagonX className="size-5" />} onClick={() => onInterrupt(step.dest)}>
            {t.handoff.interruptAndHandOff}
          </Button>
          <Button block variant="ghost" onClick={onBack}>
            {t.back}
          </Button>
        </div>
      );
    case 'confirm':
      return <Confirm exp={step.exp} dest={step.dest} onSend={() => onSend(step.dest, step.exp)} onBack={onBack} />;
    case 'sending': {
      const pct = step.total ? Math.floor((step.sent / step.total) * 100) : 0;
      return (
        <div className="space-y-3">
          <div className="text-base">{t.handoff.sending(step.dest.name)}</div>
          <div className="h-2 overflow-hidden rounded-full bg-surface-2" role="progressbar" aria-valuemin={0} aria-valuemax={100} aria-valuenow={pct}>
            <div className="h-full bg-accent transition-[width]" style={{ width: `${pct}%` }} />
          </div>
          <div className="text-xs tabular-nums text-muted">
            {byteSize(step.sent)} / {byteSize(step.total)} · {pct}%
          </div>
          <Button block variant="outline" onClick={onCancel}>
            {t.cancel}
          </Button>
        </div>
      );
    }
    case 'finishing':
      return <Working text={t.handoff.finishing(step.dest.name)} />;
    case 'done': {
      const r = step.result.result ?? null;
      const imported = step.result.state === 'imported';
      const pane = imported && typeof r?.pane === 'string' ? r.pane : null;
      return (
        <div className="space-y-3">
          <div className="flex items-center gap-2 text-lg font-medium">
            <CheckCircle2 className="size-6 text-ok" />
            {imported ? t.handoff.success(step.dest.name) : t.handoff.delivered(step.dest.name)}
          </div>
          {step.result.state === 'pending' && <Notice>{t.handoff.pending}</Notice>}
          {step.result.state === 'importing' && <Notice>{t.handoff.importing}</Notice>}
          {step.result.state === 'failed' && (
            <Notice tone="warn">
              {t.handoff.importFailed} {step.result.record?.error?.message ?? ''}
            </Notice>
          )}
          {imported && r && (
            <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
              {r.branch && (
                <>
                  <dt className="text-muted">{t.handoff.newBranch}</dt>
                  <dd className="font-mono">{r.branch}</dd>
                </>
              )}
              {r.worktree && (
                <>
                  <dt className="text-muted">{t.handoff.worktree}</dt>
                  <dd className="break-all font-mono">{r.worktree}</dd>
                </>
              )}
            </dl>
          )}
          {r?.agent_error && (
            <Notice tone="warn">
              {t.handoff.agentError} {r.agent_error.message ?? r.agent_error.data?.kind ?? ''}
            </Notice>
          )}
          {pane && !step.dest.invite && (
            <Button
              block
              size="lg"
              variant="primary"
              onClick={() => {
                onClose();
                navigate({ name: 'pane', host: step.dest.hostId, pane, view: 'term' });
              }}
            >
              {t.handoff.openOn(step.dest.name)}
            </Button>
          )}
          <Button block variant="ghost" onClick={onClose}>
            {t.done}
          </Button>
        </div>
      );
    }
    case 'unknown':
      return (
        <div className="space-y-3">
          <Notice tone="warn">{t.handoff.unknown}</Notice>
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

function Working({ text }: { text: string }) {
  return (
    <div className="flex items-center gap-3 py-6 text-base">
      <Spinner />
      {text}
    </div>
  );
}

function Confirm({ exp, dest, onSend, onBack }: { exp: ExportedHandoff; dest: Dest; onSend(): void; onBack(): void }) {
  const s = handoffSummary(exp.manifest);
  return (
    <div className="space-y-3">
      <div className="text-base font-medium">{t.handoff.confirmTitle}</div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-1 text-sm">
        <Item label={t.handoff.repo}>
          {s.repo}
          {s.origin && <div className="break-all font-mono text-2xs text-muted">{s.origin}</div>}
        </Item>
        <Item label={t.handoff.branch}>
          <span className="font-mono">{s.branch ?? exp.manifest.head.slice(0, 10)}</span>
        </Item>
        {s.harness && <Item label={t.handoff.harness}>{harnessLabel(s.harness)}</Item>}
      </dl>
      <div className={cx('text-sm', s.resumable ? 'text-ok' : 'text-muted')}>{s.resumable ? t.handoff.resumable : t.handoff.notResumable}</div>
      {s.untracked > 0 && <div className="text-sm text-muted">{t.handoff.untracked(s.untracked)}</div>}
      {s.secrets.length > 0 && (
        <Notice tone="warn">
          <div className="flex items-center gap-1.5 font-medium">
            <KeyRound className="size-4" />
            {t.handoff.secrets}
          </div>
          <ul className="mt-1 font-mono text-xs">
            {s.secrets.map((p) => (
              <li key={p} className="break-all">
                {p}
              </li>
            ))}
          </ul>
        </Notice>
      )}
      {s.otherSkipped.length > 0 && (
        <div className="text-xs text-muted">
          <div>{t.handoff.skipped}</div>
          <ul className="font-mono">
            {s.otherSkipped.map((x) => (
              <li key={x.path} className="break-all">
                {x.path} — {x.reason}
              </li>
            ))}
          </ul>
        </div>
      )}
      {s.redactions > 0 && <div className="text-sm text-muted">{t.handoff.redactions(s.redactions)}</div>}
      <div className="text-xs text-faint">{t.handoff.size(byteSize(exp.size))}</div>
      <Button block size="lg" variant="primary" icon={<Send className="size-5" />} onClick={onSend}>
        {t.handoff.send(dest.name)}
      </Button>
      <Button block variant="ghost" onClick={onBack}>
        {t.cancel}
      </Button>
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
