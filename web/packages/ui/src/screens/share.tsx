// Live share (spec 16 §15.1): a scoped, expiring bearer invitation for a pane or its workspace,
// and the "Receive a handoff" invitation (§15.2) shown in Settings.

import { useState } from 'react';
import { Link2 } from 'lucide-react';
import { displayName } from '@vibeke/core';
import { useApp, useHost, useNow } from '../app/hooks';
import { InviteLink, type Invite } from '../components/invite';
import { Button, Notice, Segmented, Sheet, TextField } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { shortDuration, whenText } from '../lib/format';
import type { PaneRow } from '../lib/tree';

const DURATIONS = ['1800', '7200', '28800', '86400'] as const;
type Duration = (typeof DURATIONS)[number];

export function ShareSheet({ row, open, onClose }: { row: PaneRow; open: boolean; onClose(): void }) {
  const app = useApp();
  const host = useHost(row.host);
  const hostName = host?.info?.host_name ?? host?.record.name ?? row.hostName;
  const [scope, setScope] = useState<'view' | 'approve'>('view');
  const [ttl, setTtl] = useState<Duration>('7200');
  const [what, setWhat] = useState<'pane' | 'workspace'>('pane');
  const [label, setLabel] = useState('');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [invite, setInvite] = useState<(Invite & { expiresAt?: number; lasts: number }) | null>(null);
  const now = useNow(60_000);

  const close = () => {
    setInvite(null);
    setErr(null);
    onClose();
  };

  const create = async () => {
    const conn = app.conn(row.host);
    if (!conn) return;
    setBusy(true);
    setErr(null);
    try {
      const fallbackLabel = what === 'pane' ? (row.pane.title ?? row.pane.auto_title) : row.workspace ? displayName(row.workspace) : undefined;
      const r = await conn.request('share.create', {
        kind: 'share',
        scope,
        ttl_s: Number(ttl),
        ...(what === 'pane' ? { pane: row.pane.id } : { workspace: row.pane.workspace }),
        ...((label.trim() || fallbackLabel) ? { label: (label.trim() || fallbackLabel)!.slice(0, 80) } : {}),
      });
      setInvite({ link: r.link, openBy: r.open_by, expiresAt: r.expires_at, lasts: r.expires_after_s ?? Number(ttl) });
      app.haptic('success');
    } catch (e) {
      setErr(errorMessage(e));
      app.haptic('error');
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet open={open} onClose={close} title={t.share.title}>
      {invite ? (
        <div className="space-y-3">
          <InviteLink invite={invite} shareTitle={t.share.shareText(hostName)} note={t.share.note} />
          <div className="text-[12px] text-muted">
            {invite.expiresAt !== undefined ? t.share.accessUntil(whenText(invite.expiresAt * 1000, now)) : t.share.lasts(shortDuration(invite.lasts * 1000))}
          </div>
          <Button block variant="ghost" onClick={() => setInvite(null)}>
            {t.share.newLink}
          </Button>
        </div>
      ) : (
        <div className="space-y-4">
          <Field label={t.share.what}>
            <Segmented<'pane' | 'workspace'>
              label={t.share.what}
              value={what}
              onChange={setWhat}
              options={[
                { value: 'pane', label: t.share.thisPane },
                { value: 'workspace', label: row.workspace ? displayName(row.workspace) : t.share.workspace },
              ]}
            />
          </Field>
          <Field label={t.share.scope}>
            <Segmented<'view' | 'approve'> label={t.share.scope} value={scope} onChange={setScope} options={(['view', 'approve'] as const).map((v) => ({ value: v, label: t.share.scopes[v]! }))} />
          </Field>
          <Field label={t.share.duration}>
            <Segmented<Duration> label={t.share.duration} value={ttl} onChange={setTtl} options={DURATIONS.map((v) => ({ value: v, label: t.share.durations[v]! }))} />
          </Field>
          <TextField label={t.share.label} placeholder={t.share.labelPlaceholder} value={label} maxLength={80} onChange={(e) => setLabel(e.target.value)} />
          <Notice>{t.share.note}</Notice>
          {err && <Notice tone="danger">{err}</Notice>}
          <Button block variant="primary" size="lg" busy={busy} icon={<Link2 className="size-5" />} onClick={() => void create()}>
            {t.share.create}
          </Button>
        </div>
      )}
    </Sheet>
  );
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="space-y-1">
      <div className="text-[13px] text-muted">{label}</div>
      <div className="overflow-x-auto">{children}</div>
    </div>
  );
}

const HANDOFF_TTLS = ['3600', '86400', '604800'] as const;
const HANDOFF_TTL_LABELS: Record<(typeof HANDOFF_TTLS)[number], string> = { '3600': '1 h', '86400': '1 day', '604800': '7 days' };

/** Settings → Receive a handoff: an invitation that lets a teammate's app send work to this host. */
export function ReceiveHandoff({ hostId, hostName }: { hostId: string; hostName: string }) {
  const app = useApp();
  const [ttl, setTtl] = useState<(typeof HANDOFF_TTLS)[number]>('86400');
  const [busy, setBusy] = useState(false);
  const [err, setErr] = useState<string | null>(null);
  const [invite, setInvite] = useState<Invite | null>(null);
  const create = async () => {
    const conn = app.conn(hostId);
    if (!conn) return;
    setBusy(true);
    setErr(null);
    try {
      const r = await conn.request('share.create', { kind: 'handoff', ttl_s: Number(ttl) });
      setInvite({ link: r.link, openBy: r.open_by });
    } catch (e) {
      setErr(errorMessage(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="space-y-3 px-4 py-3">
      <div className="text-[13px] text-muted">{t.settings.receiveHandoffHint(hostName)}</div>
      {invite ? (
        <>
          <InviteLink invite={invite} shareTitle={t.pair.handoffFrom(hostName)} note={t.share.note} />
          <Button block variant="ghost" onClick={() => setInvite(null)}>
            {t.share.newLink}
          </Button>
        </>
      ) : (
        <>
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-[13px] text-muted">{t.settings.receiveValid}</span>
            <Segmented<(typeof HANDOFF_TTLS)[number]> label={t.settings.receiveValid} value={ttl} onChange={setTtl} options={HANDOFF_TTLS.map((v) => ({ value: v, label: HANDOFF_TTL_LABELS[v] }))} />
          </div>
          {err && <Notice tone="danger">{err}</Notice>}
          <Button size="sm" variant="primary" busy={busy} icon={<Link2 className="size-4" />} onClick={() => void create()}>
            {t.settings.receiveCreate}
          </Button>
        </>
      )}
    </div>
  );
}
