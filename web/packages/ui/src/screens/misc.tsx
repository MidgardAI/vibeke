// Small screens: Crew (hosts), deep-link interaction and run routes, tour, idle lock.

import { useEffect, useId, useState } from 'react';
import { ArrowLeft, Server } from 'lucide-react';
import { useApp, useHost, useHosts, useInboxItems, usePrefs, useTree } from '../app/hooks';
import { InteractionCard } from '../components/interaction-card';
import { Dialog } from '../components/dialog';
import { Button, Card, Dot, Empty, IconButton, Spinner } from '../components/ui';
import { t } from '../i18n';
import { IdleLock } from '../lib/idle-lock';
import { useStore } from '../lib/store';
import { navigate } from '../router';
import { useWide } from '../app/shell';

export function CrewScreen() {
  const app = useApp();
  const hosts = useHosts();
  const tree = useTree();
  if (!hosts.length)
    return <Empty icon={<Server className="size-10" />} title={t.crew.none} action={<Button variant="primary" onClick={() => navigate({ name: 'pair', d: null })}>{t.crew.pair}</Button>} />;
  return (
    <div className="space-y-2 p-3">
      {tree.hosts.map((g) => {
        const h = g.host;
        const agents = h.dashboard?.runs.filter((r) => r.ended_at_ms === null).length ?? 0;
        return (
          <Card key={h.record.host_id} className="flex items-center gap-3 p-3.5">
            <Dot tone={h.status === 'online' ? 'ok' : h.status === 'connecting' ? 'warn' : 'danger'} />
            <div className="min-w-0 flex-1">
              <div className="font-medium">{h.info?.host_name ?? h.record.name}</div>
              <div className="text-xs text-muted">
                {h.status} · {t.crew.agents(agents)}
              </div>
            </div>
            {g.needsYou > 0 && <span className="rounded-full bg-need-strong px-2 py-0.5 text-xs font-semibold text-black">{t.crew.needYou(g.needsYou)}</span>}
            {h.status !== 'online' && (
              <Button size="sm" variant="outline" onClick={() => app.conn(h.record.host_id)?.reconnectNow()}>
                {t.retry}
              </Button>
            )}
          </Card>
        );
      })}
      <Button block variant="secondary" onClick={() => navigate({ name: 'pair', d: null })}>
        {t.settings.pairAnother}
      </Button>
    </div>
  );
}

export function InteractionRoute({ host, id, preselect }: { host: string; id: string; preselect: 'allow' | 'deny' | null }) {
  const items = useInboxItems();
  const h = useHost(host);
  const item = items.find((x) => x.host_id === host && x.interaction.id === id);
  const done = h?.dashboard?.interactions.find((i) => i.id === id);
  const wide = useWide();
  return (
    <div className="flex h-full flex-col pt-safe">
      <div className={`titlebar flex items-center gap-1 px-1 py-1 ${wide ? '' : 'titlebar-inset'}`}>
        <IconButton label={t.back} onClick={() => navigate({ name: 'inbox' })}>
          <ArrowLeft className="size-5" />
        </IconButton>
        <div className="text-base font-semibold">{t.tabs.inbox}</div>
      </div>
      <div className="flex-1 overflow-y-auto p-3">
        {item ? (
          <InteractionCard item={item} preselect={preselect} showHost />
        ) : !h?.dashboard ? (
          <div className="flex justify-center py-10">
            <Spinner />
          </div>
        ) : (
          <Empty
            title={t.inbox.gone}
            hint={done?.answered_by ? t.inbox.answeredBy(done.answered_by) : undefined}
            action={<Button onClick={() => navigate({ name: 'inbox' })}>{t.tabs.inbox}</Button>}
          />
        )}
      </div>
    </div>
  );
}

export function RunRoute({ host, run }: { host: string; run: string }) {
  const h = useHost(host);
  useEffect(() => {
    const r = h?.dashboard?.runs.find((x) => x.id === run);
    if (r) navigate({ name: 'pane', host, pane: r.pane, view: 'term' }, { replace: true });
    else if (h?.dashboard) navigate({ name: 'panes' }, { replace: true });
  }, [h?.dashboard, host, run]);
  return (
    <div className="flex h-full items-center justify-center">
      <Spinner />
    </div>
  );
}

export function Tour() {
  const app = useApp();
  const prefs = usePrefs();
  const [i, setI] = useState(0);
  const titleId = useId();
  if (prefs.tourDone) return null;
  const steps = t.tour.steps;
  const step = steps[i]!;
  const last = i === steps.length - 1;
  const finish = () => app.prefs.patch({ tourDone: true });
  return (
    <Dialog
      open
      onClose={finish}
      labelledBy={titleId}
      className="fixed inset-0 z-50 flex items-end sm:items-center sm:justify-center sm:p-6"
      panelClassName="animate-sheet w-full rounded-t-3xl bg-surface p-6 pb-safe shadow-2xl outline-none sm:max-w-md sm:rounded-2xl sm:border sm:border-border"
    >
        <div className="mb-3 flex gap-1">
          {steps.map((_, j) => (
            <span key={j} className={`h-1 flex-1 rounded-full ${j <= i ? 'bg-accent' : 'bg-surface-2'}`} />
          ))}
        </div>
        <h2 id={titleId} className="text-lg font-semibold tracking-tight">{step.title}</h2>
        <p className="mt-2 text-base text-muted">{step.body}</p>
        <div className="mt-5 flex gap-2 pb-2">
          <Button variant="ghost" onClick={finish}>
            {t.tour.skip}
          </Button>
          <span className="flex-1" />
          {last ? (
            <>
              {app.platform.push && app.push.state === 'off' && !app.platform.notifications?.needsInstallForPush() && (
                <Button
                  variant="outline"
                  onClick={async () => {
                    finish();
                    const p = await app.platform.notifications?.requestPermission();
                    if (p === 'granted') await app.push.enable().catch(() => {});
                  }}
                >
                  {t.tour.enablePush}
                </Button>
              )}
              <Button variant="primary" onClick={finish}>
                {t.tour.finish}
              </Button>
            </>
          ) : (
            <Button variant="primary" onClick={() => setI(i + 1)}>
              {t.tour.next}
            </Button>
          )}
        </div>
    </Dialog>
  );
}

export const IDLE_LOCK_MS = 30 * 60_000;

/**
 * Idle lock (spec 16 §9.1 Shell): 30 min visible but untouched → pause polling. No timers while
 * the window is hidden; the lock timer resumes (re-armed) when it is shown.
 */
export function useIdleLock(): void {
  const app = useApp();
  useEffect(() => {
    const lock = new IdleLock({ clock: app.platform.clock, visible: app.visible, locked: app.locked, idleMs: IDLE_LOCK_MS }).start();
    const touch = () => lock.touch();
    const evs = ['pointerdown', 'keydown', 'wheel', 'touchstart'] as const;
    evs.forEach((e) => window.addEventListener(e, touch, { passive: true }));
    return () => {
      evs.forEach((e) => window.removeEventListener(e, touch));
      lock.stop();
    };
  }, [app]);
}

export function IdleLockOverlay() {
  const app = useApp();
  const locked = useStore(app.locked);
  const [catching, setCatching] = useState(false);
  const titleId = useId();
  return (
    <Dialog
      open={locked || catching}
      onClose={() => {}}
      dismissable={false}
      role="alertdialog"
      labelledBy={titleId}
      className="fixed inset-0 z-[60] flex flex-col items-center justify-center bg-bg/95 p-8 text-center backdrop-blur"
      panelClassName="flex flex-col items-center gap-4 outline-none"
    >
      <h2 id={titleId} className="text-xl font-semibold tracking-tight">{catching ? t.lock.catchingUp : t.lock.title}</h2>
      {!catching && <div className="max-w-xs text-sm text-muted">{t.lock.body}</div>}
      {catching ? (
        <Spinner className="size-6" />
      ) : (
        <Button
          variant="primary"
          size="lg"
          data-autofocus
          onClick={async () => {
            setCatching(true);
            app.locked.set(false);
            await Promise.all(app.manager.connections().map((c) => (c.getSnapshot().status === 'online' ? c.refresh().catch(() => {}) : (c.reconnectNow(), undefined))));
            setCatching(false);
          }}
        >
          {t.lock.resume}
        </Button>
      )}
    </Dialog>
  );
}
