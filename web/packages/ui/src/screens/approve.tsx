// Approval requests from panes (spec 09 §3.2 "Approved calls"): a shell pane (or an agent in
// one) asked to run one specific call — send its work to another host, cancel a handoff, redeem
// a peer invitation. The card shows the host's own summary of what will happen first, then the
// pane's reason, clearly marked unverified, and decides only on an explicit tap. The inbox shows
// the cards with Approve / Deny; the review screen (`#/approve/<host>[/<request>]`, where the
// push notification lands) also offers "Always for this pane" when the host allows it.

import { ShieldAlert } from 'lucide-react';
import type { ApprovalDecision, ApprovalRequest, HostState } from '@vibeke/core';
import { useApprovalStores, useApprovals, useDeciding, useHostApprovals } from '../app/approval-stores';
import { useAllHosts, useNow } from '../app/hooks';
import { Button, Card, Empty, Notice, SectionLabel, Spinner, cx } from '../components/ui';
import { t } from '../i18n';
import { approvalTitle, decisionsFor } from '../lib/approvals';
import { relTime } from '../lib/format';
import { isOwnFullHost } from '../lib/handoff-send';
import { navigate } from '../router';

const hostName = (h: HostState): string => h.info?.host_name ?? h.record.name;

export function ApprovalScreen({ host, id }: { host: string | null; id: string | null }) {
  const hosts = useAllHosts().filter(isOwnFullHost);
  const all = useApprovals();
  const shown = host ? hosts.filter((h) => h.record.host_id === host) : hosts;
  if (host && id) return <OneApproval hostId={host} id={id} />;
  const any = shown.some((h) => (all.get(h.record.host_id)?.list.length ?? 0) > 0);
  if (!any) return <Empty icon={<ShieldAlert />} title={t.approve.empty} hint={t.approve.emptyHint} />;
  return (
    <div className="space-y-3 px-3 pb-10 pt-2 sm:px-4">
      {shown.map((h) =>
        (all.get(h.record.host_id)?.list ?? []).map((r) => (
          <ApprovalCard key={`${h.record.host_id}:${r.request}`} host={h} r={r} showHost={shown.length > 1} full />
        )),
      )}
    </div>
  );
}

function OneApproval({ hostId, id }: { hostId: string; id: string }) {
  const host = useAllHosts().find((h) => h.record.host_id === hostId);
  const data = useHostApprovals(hostId);
  if (!host || !isOwnFullHost(host)) return <Empty icon={<ShieldAlert />} title={t.approve.readOnly} />;
  const r = data.list.find((x) => x.request === id);
  if (!r) {
    if (host.status === 'online' && !data.loaded) {
      return (
        <div className="flex justify-center py-16">
          <Spinner />
        </div>
      );
    }
    return (
      <Empty
        icon={<ShieldAlert />}
        title={host.status === 'online' ? t.approve.gone : t.approve.hostOffline}
        action={
          data.list.length > 0 ? (
            <Button variant="outline" onClick={() => navigate({ name: 'approve', host: hostId, id: null })}>
              {t.approve.screenTitle}
            </Button>
          ) : undefined
        }
      />
    );
  }
  return (
    <div className="px-3 pb-10 pt-2 sm:px-4">
      {data.error && <Notice tone="danger">{data.error}</Notice>}
      <ApprovalCard host={host} r={r} showHost full />
    </div>
  );
}

/**
 * One request. `full` (the review screen) also offers "Always for this pane" when allowed; the
 * inbox card offers Approve / Deny and a link to the review screen.
 */
export function ApprovalCard({ host, r, showHost, full = false }: { host: HostState; r: ApprovalRequest; showHost: boolean; full?: boolean }) {
  const stores = useApprovalStores();
  const deciding = useDeciding();
  const now = useNow(60_000);
  const hostId = host.record.host_id;
  const busy = deciding.has(`${hostId}:${r.request}`);
  const online = host.status === 'online';
  const decide = (d: ApprovalDecision) => void stores.decide(hostId, r, d);
  const offered = decisionsFor(r).filter((d) => full || d !== 'always');
  const peer = r.peer ? r.peer.name || r.peer.id : null;
  return (
    <Card className="space-y-3 p-4">
      <div className="flex items-start gap-3">
        <ShieldAlert className="mt-0.5 size-5 shrink-0 text-need" />
        <div className="min-w-0 flex-1">
          <div className="text-base font-semibold">{approvalTitle(r)}</div>
          <div className="text-xs text-muted">{t.approve.asked(relTime(r.created_at_ms, now), showHost ? hostName(host) : r.request)}</div>
        </div>
      </div>
      <div className="space-y-1">
        <div className="text-xs text-muted">{t.approve.what}</div>
        <div className="whitespace-pre-wrap break-words text-sm font-medium">{r.summary}</div>
      </div>
      <div className="space-y-1">
        <div className="text-xs text-muted">{t.approve.reason}</div>
        {r.reason ? (
          <blockquote className="max-h-32 overflow-y-auto whitespace-pre-wrap break-words rounded-lg border-l-2 border-border-strong bg-surface-2 px-3 py-2 text-sm italic">{r.reason}</blockquote>
        ) : (
          <div className="text-sm text-faint">{t.approve.noReason}</div>
        )}
      </div>
      {full && <div className="text-xs text-muted">{r.always_allowed ? t.approve.alwaysHint(peer) : t.approve.onceOnly}</div>}
      {!online && <Notice tone="warn">{t.approve.hostOffline}</Notice>}
      <div className="flex flex-wrap gap-2">
        {offered.map((d) => (
          <Button
            key={d}
            variant={d === 'deny' ? 'outline' : d === 'approve' ? 'primary' : 'secondary'}
            busy={busy}
            disabled={!online}
            onClick={() => decide(d)}
          >
            {d === 'approve' ? (full && r.always_allowed ? t.approve.approveOnce : t.approve.approve) : d === 'always' ? t.approve.always : t.approve.deny}
          </Button>
        ))}
        {!full && (
          <Button variant="ghost" className={cx('ml-auto')} onClick={() => navigate({ name: 'approve', host: hostId, id: r.request })}>
            {t.approve.review}
          </Button>
        )}
      </div>
    </Card>
  );
}

/** The inbox's approval cards (every own host), above the agents' interactions. */
export function InboxApprovals({ showHost }: { showHost: boolean }) {
  const hosts = useAllHosts().filter(isOwnFullHost);
  const all = useApprovals();
  const rows = hosts.flatMap((h) => (all.get(h.record.host_id)?.list ?? []).map((r) => ({ h, r })));
  if (rows.length === 0) return null;
  return (
    <>
      {rows.length > 1 && <SectionLabel>{t.approve.screenTitle}</SectionLabel>}
      {rows.map(({ h, r }) => (
        <div key={`approval:${h.record.host_id}:${r.request}`}>
          <ApprovalCard host={h} r={r} showHost={showHost} />
        </div>
      ))}
    </>
  );
}
