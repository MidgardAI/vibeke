// Sandboxes (spec 17 §6, route `#/sandboxes`): every cloud box of the user's own hosts, grouped by
// provider. Each row shows the state, who owns it, its task and panes, its age and last activity,
// and a marker when it holds work that is not on the host. Row actions follow the box's
// capabilities. Destroying a box with unsynced work asks first; "Clean up…" previews what
// `cloud.prune` would destroy before it does.

import { useState } from 'react';
import { Cloud, RefreshCw, Trash2 } from 'lucide-react';
import { RpcError, type CloudBox, type CloudProvider, type HostConnectionApi } from '@vibeke/core';
import { useCloudStores, useHostCloud, type HostCloud } from '../app/cloud-stores';
import { useAllHosts, useApp, useNow } from '../app/hooks';
import { requestCloudAuth, withCloudAuth } from '../components/cloud-auth';
import { Button, Card, Empty, Notice, Pill, Sheet, Spinner } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { authEnvVar, boxActions, boxCounts, groupBoxes, hasUnsynced, tryDestroy, type BoxAction, type ProviderGroup } from '../lib/cloud';
import { relTime } from '../lib/format';
import { isOwnFullHost } from '../lib/handoff-send';
import { navigate } from '../router';
import { CloudSheet } from './cloud-send';

// ---- presentation (props only, so it renders in tests) --------------------------------------

export function SandboxGroups({
  groups,
  now,
  busy,
  onAction,
  onSignIn,
  onSignOut,
}: {
  groups: ProviderGroup[];
  now: number;
  busy: string | null;
  onAction(box: CloudBox, a: BoxAction): void;
  onSignIn(p: CloudProvider): void;
  onSignOut(p: CloudProvider): void;
}) {
  return (
    <div className="space-y-4">
      {groups.map((g) => (
        <section key={g.provider} aria-label={g.info?.label ?? g.provider}>
          <div className="flex items-center gap-2 px-1 pb-1.5">
            <Cloud className="size-4 text-muted" />
            <h2 className="text-sm font-semibold">{g.info?.label ?? g.provider}</h2>
            {g.info && <AuthState p={g.info} onSignIn={() => onSignIn(g.info!)} onSignOut={() => onSignOut(g.info!)} />}
          </div>
          {g.boxes.length === 0 ? (
            <div className="px-1 text-sm text-muted">{t.cloud.empty}</div>
          ) : (
            <Card className="divide-y divide-border">
              {g.boxes.map((b) => (
                <BoxRow key={b.box} b={b} now={now} busy={busy === b.box} onAction={(a) => onAction(b, a)} />
              ))}
            </Card>
          )}
        </section>
      ))}
    </div>
  );
}

function AuthState({ p, onSignIn, onSignOut }: { p: CloudProvider; onSignIn(): void; onSignOut(): void }) {
  const env = authEnvVar(p);
  return (
    <div className="ml-auto flex items-center gap-2 text-xs text-muted">
      <span>{p.auth.state === 'ok' ? t.cloud.signedIn(p.auth.account ?? '') : p.auth.state === 'invalid' ? t.cloud.invalid : t.cloud.signedOut}</span>
      {p.auth.state !== 'ok' && (
        <Button size="sm" variant="outline" onClick={onSignIn}>
          {t.cloud.signIn}
        </Button>
      )}
      {p.auth.state === 'ok' && !env && (
        <Button size="sm" variant="ghost" onClick={onSignOut}>
          {t.cloud.signOut}
        </Button>
      )}
    </div>
  );
}

const ACTION_LABEL: Record<BoxAction, string> = {
  open: t.cloud.open,
  bring_back: t.cloud.bringBack,
  suspend: t.cloud.suspend,
  resume: t.cloud.resume,
  checkpoint: t.cloud.checkpoint,
  adopt: t.cloud.adopt,
  forget: t.cloud.forget,
  destroy: t.cloud.destroy,
};

function BoxRow({ b, now, busy, onAction }: { b: CloudBox; now: number; busy: boolean; onAction(a: BoxAction): void }) {
  const actions = boxActions(b);
  return (
    <div className="space-y-1.5 p-3" data-box={b.box}>
      <div className="flex flex-wrap items-center gap-2">
        <span className="min-w-0 truncate font-medium">{b.task || b.name}</span>
        <Pill>{b.state}</Pill>
        <Pill>{t.cloud.ownership[b.ownership] ?? b.ownership}</Pill>
        {hasUnsynced(b) && (
          <Pill className="border-warn/50 text-warn" >
            {t.cloud.unsynced}
          </Pill>
        )}
      </div>
      <div className="text-xs text-muted">
        {[
          b.task ? null : t.cloud.noTask,
          t.cloud.panes(b.panes.length),
          b.created_at ? t.cloud.created(relTime(b.created_at * 1000, now)) : null,
          b.last_activity_at ? t.cloud.active(relTime(b.last_activity_at * 1000, now)) : null,
          hasUnsynced(b) ? b.unsynced!.summary : null,
        ]
          .filter(Boolean)
          .join(' · ')}
      </div>
      <div className="flex flex-wrap gap-1.5 pt-0.5">
        {actions.map((a) => (
          <Button key={a} size="sm" variant={a === 'destroy' ? 'outline' : 'secondary'} busy={busy} onClick={() => onAction(a)} className={a === 'destroy' ? 'text-danger' : undefined}>
            {ACTION_LABEL[a]}
          </Button>
        ))}
      </div>
    </div>
  );
}

// ---- the screen -----------------------------------------------------------------------------

export function SandboxesScreen() {
  const hosts = useAllHosts().filter(isOwnFullHost);
  const stores = useCloudStores();
  const [spin, setSpin] = useState(false);
  const all = hosts.flatMap((h) => stores.hosts.get().get(h.record.host_id)?.boxes ?? []);
  const { running, idle } = boxCounts(all);
  const refresh = async () => {
    setSpin(true);
    await Promise.all(hosts.map((h) => stores.refresh(h.record.host_id, { refresh: true, verify: true })));
    setSpin(false);
  };
  return (
    <div className="space-y-4 p-3 pb-8 sm:p-4">
      <div className="flex items-center gap-2">
        <div className="min-w-0 flex-1 text-sm text-muted">{t.cloud.sandboxesHint}</div>
        <Button size="sm" variant="outline" icon={<RefreshCw className="size-4" />} busy={spin} onClick={() => void refresh()} aria-label={t.retry} />
      </div>
      {hosts.length === 0 && <Empty icon={<Cloud />} title={t.cloud.empty} hint={t.cloud.noProviders} />}
      {hosts.map((h) => (
        <HostSandboxes key={h.record.host_id} hostId={h.record.host_id} name={h.info?.host_name ?? h.record.name} many={hosts.length > 1} />
      ))}
      <div className="border-t border-border pt-2 text-xs text-muted">{t.cloud.footer(running, idle)}</div>
    </div>
  );
}

type Dialog =
  | { k: 'destroy'; box: CloudBox; unsynced: string | null }
  | { k: 'bring_back'; box: CloudBox }
  | { k: 'prune'; candidates: CloudBox[]; skipped: { box: string; reason: string }[] }
  | null;

function HostSandboxes({ hostId, name, many }: { hostId: string; name: string; many: boolean }) {
  const app = useApp();
  const stores = useCloudStores();
  const cloud: HostCloud = useHostCloud(hostId);
  const now = useNow(30_000) ;
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [dialog, setDialog] = useState<Dialog>(null);
  const conn = (): HostConnectionApi | undefined => app.conn(hostId);
  const groups = groupBoxes(cloud.providers, cloud.boxes);

  const fail = (e: unknown) => setError(e instanceof RpcError || e instanceof Error ? errorMessage(e) : t.cloud.actionFailed);

  const act = async (b: CloudBox, a: BoxAction) => {
    const c = conn();
    if (!c) return;
    setError(null);
    if (a === 'open') {
      const pane = b.panes[0];
      if (pane) navigate({ name: 'pane', host: hostId, pane, view: 'term' });
      return;
    }
    if (a === 'bring_back') return setDialog({ k: 'bring_back', box: b });
    if (a === 'destroy') return setDialog({ k: 'destroy', box: b, unsynced: hasUnsynced(b) ? b.unsynced!.summary : null });
    setBusy(b.box);
    try {
      const run = () => {
        switch (a) {
          case 'suspend':
            return c.request('cloud.box.suspend', { box: b.box });
          case 'resume':
            return c.request('cloud.box.resume', { box: b.box });
          case 'checkpoint':
            return c.request('cloud.box.checkpoint', { box: b.box });
          case 'adopt':
            return c.request('cloud.box.adopt', { box: b.box }, { timeoutMs: 120_000 });
          default:
            return c.request('cloud.box.forget', { box: b.box });
        }
      };
      await withCloudAuth(c, b.provider, run);
      if (a === 'forget') stores.dropBox(hostId, b.box);
      void stores.refresh(hostId);
    } catch (e) {
      fail(e);
    } finally {
      setBusy(null);
    }
  };

  const destroy = async (b: CloudBox, force: boolean) => {
    const c = conn();
    if (!c) return;
    setBusy(b.box);
    try {
      const r = await withCloudAuth(c, b.provider, () => tryDestroy(c, b.box, force));
      if (r.k === 'unsynced') return setDialog({ k: 'destroy', box: b, unsynced: r.unsynced?.summary || r.message });
      stores.dropBox(hostId, b.box);
      setDialog(null);
    } catch (e) {
      setDialog(null);
      fail(e);
    } finally {
      setBusy(null);
    }
  };

  const prune = async () => {
    const c = conn();
    if (!c) return;
    setError(null);
    try {
      const r = await c.request('cloud.prune', { ownership: ['orphaned', 'idle'], dry_run: true });
      setDialog({ k: 'prune', candidates: r.candidates ?? [], skipped: r.skipped ?? [] });
    } catch (e) {
      fail(e);
    }
  };

  const confirmPrune = async () => {
    const c = conn();
    if (!c) return;
    try {
      const r = await c.request('cloud.prune', { ownership: ['orphaned', 'idle'] }, { timeoutMs: 180_000 });
      app.toast(t.cloud.cleanedUp(r.destroyed?.length ?? 0), 'ok');
      void stores.refresh(hostId, { refresh: true });
    } catch (e) {
      fail(e);
    }
    setDialog(null);
  };

  const signIn = async (p: CloudProvider) => {
    const c = conn();
    if (c) await requestCloudAuth(c, p.id, p.methods, p.label);
  };
  const signOut = async (p: CloudProvider) => {
    const c = conn();
    if (!c) return;
    try {
      await c.request('cloud.auth.clear', { provider: p.id });
      void stores.refreshProviders(hostId);
    } catch (e) {
      fail(e);
    }
  };

  return (
    <div className="space-y-3">
      {many && <div className="text-xs font-medium uppercase text-faint">{name}</div>}
      {!cloud.loaded && <Spinner />}
      {cloud.unsupported && <Notice>{t.cloud.noProviders}</Notice>}
      {cloud.error && <Notice tone="warn">{cloud.error}</Notice>}
      {cloud.errors.length > 0 && (
        <Notice tone="warn">
          {t.cloud.sandboxesErrors}: {cloud.errors.map((e) => `${e.provider} (${e.message})`).join(', ')}
        </Notice>
      )}
      {error && <Notice tone="danger">{error}</Notice>}
      {cloud.loaded && !cloud.unsupported && (
        <>
          <SandboxGroups groups={groups} now={now} busy={busy} onAction={(b, a) => void act(b, a)} onSignIn={(p) => void signIn(p)} onSignOut={(p) => void signOut(p)} />
          {cloud.boxes.length > 0 && (
            <Button size="sm" variant="outline" icon={<Trash2 className="size-4" />} onClick={() => void prune()}>
              {t.cloud.cleanUp}
            </Button>
          )}
        </>
      )}

      {dialog?.k === 'destroy' && (
        <Sheet open role="alertdialog" onClose={() => setDialog(null)} title={`${t.cloud.destroy} ${dialog.box.task || dialog.box.name}?`}>
          <div className="space-y-3">
            {dialog.unsynced && (
              <Notice tone="warn">
                {t.cloud.unsyncedTitle}: {dialog.unsynced}
              </Notice>
            )}
            {dialog.unsynced && dialog.box.panes.length > 0 && (
              <Button block variant="primary" onClick={() => setDialog({ k: 'bring_back', box: dialog.box })}>
                {t.cloud.bringBackFirst}
              </Button>
            )}
            <Button block variant="danger" busy={busy === dialog.box.box} onClick={() => void destroy(dialog.box, !!dialog.unsynced)}>
              {dialog.unsynced ? t.cloud.destroyAnyway : t.cloud.destroy}
            </Button>
            <Button block variant="ghost" onClick={() => setDialog(null)}>
              {t.cancel}
            </Button>
          </div>
        </Sheet>
      )}
      {dialog?.k === 'bring_back' && <CloudSheet mode="bring_back" host={hostId} box={dialog.box.box} open onClose={() => setDialog(null)} />}
      {dialog?.k === 'prune' && (
        <Sheet open role="alertdialog" onClose={() => setDialog(null)} title={t.cloud.cleanUpTitle}>
          <div className="space-y-3">
            {dialog.candidates.length === 0 ? (
              <Notice>{t.cloud.cleanUpNone}</Notice>
            ) : (
              <>
                <div className="text-sm text-muted">{t.cloud.cleanUpIntro(dialog.candidates.length)}</div>
                <ul className="space-y-1 text-sm">
                  {dialog.candidates.map((b) => (
                    <li key={b.box} className="flex items-center gap-2">
                      <span className="min-w-0 flex-1 truncate">{b.task || b.name}</span>
                      <Pill>{t.cloud.ownership[b.ownership] ?? b.ownership}</Pill>
                    </li>
                  ))}
                </ul>
              </>
            )}
            {dialog.skipped.length > 0 && (
              <div className="text-xs text-muted">
                {t.cloud.cleanUpSkipped}: {dialog.skipped.map((s) => `${s.box} (${s.reason})`).join(', ')}
              </div>
            )}
            {dialog.candidates.length > 0 && (
              <Button block variant="danger" onClick={() => void confirmPrune()}>
                {t.cloud.destroyAll(dialog.candidates.length)}
              </Button>
            )}
            <Button block variant="ghost" onClick={() => setDialog(null)}>
              {dialog.candidates.length > 0 ? t.cancel : t.close}
            </Button>
          </div>
        </Sheet>
      )}
    </div>
  );
}
