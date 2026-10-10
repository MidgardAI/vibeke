// Goals from the phone: the list of every host's goals and one goal's screen (title, text, repo,
// state, progress, plan steps). A plan that waits for approval can be approved or refused. The
// host has no "reject plan": cancelling the goal is the way to refuse it.

import { useCallback, useEffect, useRef, useState } from 'react';
import { Check, CircleDashed, Target } from 'lucide-react';
import { RpcError, type Goal, type GoalView } from '@vibeke/core';
import { goalsHost, useGoalLists } from '../app/goals';
import { useAllHosts, useApp } from '../app/hooks';
import { Button, Card, Chip, Empty, Notice, Row, SectionLabel, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { goalFromStale, goalTone, planSteps, planWaits, progressFraction, sortGoals, stepStatus } from '../lib/goals';
import { useStore } from '../lib/store';
import { navigate } from '../router';

const stateLabel = (s: string): string => t.goals.states[s] ?? s;

function Progress({ progress }: { progress: { done: number; total: number } }) {
  const f = progressFraction(progress);
  if (f === null) return <span className="text-xs text-muted">{t.goals.noSteps}</span>;
  return (
    <span className="flex items-center gap-2 text-xs text-muted">
      <span className="h-1.5 w-20 overflow-hidden rounded-full bg-surface-2" role="progressbar" aria-valuemin={0} aria-valuemax={progress.total} aria-valuenow={progress.done}>
        <span className="block h-full rounded-full bg-accent" style={{ width: `${Math.round(f * 100)}%` }} />
      </span>
      <span className="tabular-nums">{t.goals.progress(progress.done, progress.total)}</span>
    </span>
  );
}

export function GoalsScreen() {
  const { lists, loading } = useGoalLists(15_000);
  const hosts = useAllHosts();
  const multi = lists.length > 1;
  const any = lists.some((l) => l.goals.length > 0);
  if (loading && !any)
    return (
      <div className="flex justify-center py-16">
        <Spinner />
      </div>
    );
  if (!any) return <Empty icon={<Target />} title={t.goals.empty} hint={hosts.some(goalsHost) ? t.goals.emptyHint : t.goals.noHosts} />;
  return (
    <div className="pb-10">
      {lists.map((l) =>
        l.goals.length ? (
          <section key={l.hostId}>
            {multi && <SectionLabel>{l.hostName}</SectionLabel>}
            <div className="space-y-px px-2 pt-1">
              {sortGoals(l.goals).map((v) => (
                <Row
                  key={v.goal.id}
                  leading={<Target className="size-4" />}
                  trailing={<Chip tone={goalTone(v.goal.state)}>{stateLabel(v.goal.state)}</Chip>}
                  sub={<Progress progress={v.progress} />}
                  onClick={() => navigate({ name: 'goal', host: l.hostId, goal: v.goal.id })}
                  className="pointer-coarse:min-h-11"
                >
                  {v.goal.title || v.goal.handle}
                </Row>
              ))}
            </div>
          </section>
        ) : null,
      )}
    </div>
  );
}

export function GoalScreen({ host, goal }: { host: string; goal: string }) {
  const app = useApp();
  const hostState = useAllHosts().find((h) => h.record.host_id === host);
  const visible = useStore(app.visible);
  const [view, setView] = useState<GoalView | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [missing, setMissing] = useState(false);
  const [busy, setBusy] = useState<'approve' | 'cancel' | null>(null);
  const [changed, setChanged] = useState(false);
  const [confirmCancel, setConfirmCancel] = useState(false);
  const online = hostState?.status === 'online';
  const scope = hostState?.info?.scope ?? hostState?.record.scope ?? 'view';

  const load = useCallback(async () => {
    const conn = app.conn(host);
    if (!conn) return;
    try {
      const v = await conn.request('goal.get', { goal });
      setView(v);
      setError(null);
      setMissing(false);
    } catch (e) {
      if (e instanceof RpcError && e.kind === 'not_found') setMissing(true);
      else setError(errorMessage(e));
    }
  }, [app, host, goal]);

  useEffect(() => {
    if (!online || !visible) return;
    void load();
    const id = setInterval(() => void load(), 10_000);
    return () => clearInterval(id);
  }, [online, visible, load]);

  // The cancel confirmation lapses after a few seconds.
  const lapse = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => void (lapse.current && clearTimeout(lapse.current)), []);

  if (missing) return <Empty icon={<Target />} title={t.goals.notFound} action={<Button onClick={() => navigate({ name: 'goals' })}>{t.goals.title}</Button>} />;
  if (!view)
    return error ? (
      <div className="px-4 pt-4">
        <Notice tone="danger" action={<Button size="sm" onClick={() => void load()}>{t.retry}</Button>}>
          {error}
        </Notice>
      </div>
    ) : (
      <div className="flex justify-center py-16">
        {online || !hostState ? <Spinner /> : <span className="text-sm text-muted">{t.inbox.hostOffline}</span>}
      </div>
    );

  const g: Goal = view.goal;
  const steps = planSteps(g);
  const canAct = online && scope === 'full';
  const waits = planWaits(g);

  const approve = async () => {
    const conn = app.conn(host);
    if (!conn) return;
    setBusy('approve');
    app.haptic('tap');
    try {
      const v = await conn.request('goal.approve', { goal: g.id, plan_rev: g.plan_rev });
      setView(v);
      setChanged(false);
      app.toast(t.goals.approved, 'ok');
    } catch (e) {
      if (e instanceof RpcError && e.kind === 'stale') {
        // The plan changed (or the goal moved on) since this screen loaded: show the current one.
        const now = goalFromStale(e.data);
        if (now) setView({ goal: now, progress: view.progress });
        void load();
        setChanged(true);
      } else app.toast(errorMessage(e), 'error');
    } finally {
      setBusy(null);
    }
  };

  const cancel = async () => {
    if (!confirmCancel) {
      setConfirmCancel(true);
      lapse.current = setTimeout(() => setConfirmCancel(false), 5000);
      return;
    }
    const conn = app.conn(host);
    if (!conn) return;
    if (lapse.current) clearTimeout(lapse.current);
    setConfirmCancel(false);
    setBusy('cancel');
    try {
      const v = await conn.request('goal.cancel', { goal: g.id });
      setView({ goal: v.goal, progress: v.progress });
      app.toast(t.goals.cancelled, 'ok');
    } catch (e) {
      if (e instanceof RpcError && e.kind === 'stale') void load();
      else app.toast(errorMessage(e), 'error');
    } finally {
      setBusy(null);
    }
  };

  return (
    <div className="space-y-4 px-4 pb-10 pt-3">
      <div className="space-y-2">
        <div className="flex flex-wrap items-center gap-2">
          <h2 className="min-w-0 flex-1 text-lg font-semibold leading-snug">{g.title || g.handle}</h2>
          <Chip tone={goalTone(g.state)}>{stateLabel(g.state)}</Chip>
        </div>
        {g.repo && <div className="break-all font-mono text-xs text-muted">{g.repo}</div>}
        <Progress progress={view.progress} />
      </div>

      {g.text && <p className="whitespace-pre-wrap text-sm leading-relaxed">{g.text}</p>}

      {changed && <Notice tone="warn">{t.goals.planChanged}</Notice>}

      <section aria-label={t.goals.plan}>
        <div className="pb-1.5 text-xs font-medium text-muted">{t.goals.plan}</div>
        {steps.length === 0 ? (
          <div className="text-sm text-muted">{t.goals.noPlan}</div>
        ) : (
          <Card className="divide-y divide-border">
            {steps.map((s, i) => {
              const st = stepStatus(s);
              const done = st === 'done';
              return (
                <div key={s.id ?? i} className="flex items-start gap-3 px-3.5 py-3">
                  <span className="mt-0.5 shrink-0 text-muted">{done ? <Check className="size-4 text-ok" /> : <CircleDashed className="size-4" />}</span>
                  <div className="min-w-0 flex-1">
                    <div className={cx('text-sm font-medium leading-snug', done && 'text-muted')}>
                      <span className="mr-1.5 tabular-nums text-faint">{i + 1}.</span>
                      {s.title}
                    </div>
                    {st && !done && <div className="mt-0.5 text-xs text-muted">{t.goals.stepStates[st] ?? st}</div>}
                  </div>
                </div>
              );
            })}
          </Card>
        )}
      </section>

      {waits && !canAct && <Notice>{scope === 'view' ? t.goals.viewOnly : online ? t.goals.needsFull : t.inbox.hostOffline}</Notice>}
      {waits && canAct && (
        <div className="space-y-3">
          <Notice tone="info">{t.goals.refuseHint}</Notice>
          <div className="flex gap-2">
            <Button size="lg" className="flex-1" variant={confirmCancel ? 'danger' : 'outline'} busy={busy === 'cancel'} disabled={busy !== null} onClick={() => void cancel()}>
              {confirmCancel ? t.goals.cancelConfirm : t.goals.cancel}
            </Button>
            <Button size="lg" className="flex-1" variant="ok" icon={<Check />} busy={busy === 'approve'} disabled={busy !== null} onClick={() => void approve()}>
              {t.goals.approve}
            </Button>
          </div>
        </div>
      )}
      {g.state === 'draft' && <Notice>{t.goals.draft}</Notice>}
    </div>
  );
}
