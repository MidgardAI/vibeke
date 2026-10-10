import { useMemo, useRef, type ReactNode } from 'react';
import { Inbox as InboxIcon } from 'lucide-react';
import { groupBatches, type Batch } from '@vibeke/core';
import { useApprovalCount } from '../app/approval-stores';
import { useAnswers, useApp, useHosts, useInboxItems, useNow } from '../app/hooks';
import { BatchCard } from '../components/batch-card';
import { CatchUpSection } from '../components/catch-up';
import { InteractionCard } from '../components/interaction-card';
import { Empty } from '../components/ui';
import { t } from '../i18n';
import { InboxRetainer, itemKey, type RetainedEntry } from '../lib/retain';
import { useCatchUp } from '../lib/use-catch-up';
import { InboxApprovals } from './approve';

export function InboxScreen() {
  const app = useApp();
  const hosts = useHosts();
  const items = useInboxItems();
  useAnswers();
  const now = useNow(500);
  const retainer = useRef(new InboxRetainer());
  const showHost = hosts.length > 1;
  // Approval requests from panes (spec 09 §3.2) come first: a pane waits on each one.
  const approvals = useApprovalCount();
  // What happened while the app was in the background (cards only after a long absence).
  const catchUp = useCatchUp();

  const entries = retainer.current.update(
    items,
    (host, id) => hosts.find((h) => h.record.host_id === host)?.dashboard?.interactions.find((i) => i.id === id) ?? app.finals.get(`${host}/${id}`),
    (key) => app.answers.get(key),
    now,
  );

  // Batches only over cards that are still open and not individually being answered.
  const batches = useMemo(() => groupBatches(items.filter((it) => !app.answers.get(itemKey(it)))), [items, app.answers.getSnapshot()]);
  const batchOf = new Map<string, Batch>();
  for (const b of batches) for (const it of b.items) batchOf.set(itemKey(it), b);

  const rendered = new Set<Batch>();
  const rows: { key: string; node: ReactNode }[] = [];
  for (const e of entries) {
    const b = e.mode === 'open' ? batchOf.get(e.key) : undefined;
    if (b) {
      if (rendered.has(b)) continue;
      rendered.add(b);
      rows.push({ key: `batch:${b.fingerprint}:${b.host_id}`, node: <BatchCard batch={b} showHost={showHost} /> });
      continue;
    }
    rows.push({ key: e.key, node: <EntryCard entry={e} showHost={showHost} /> });
  }

  if (rows.length === 0 && approvals === 0 && catchUp.cards.length === 0) {
    return <Empty icon={<InboxIcon className="size-10" />} title={t.inbox.empty} hint={t.inbox.emptyHint} />;
  }
  return (
    <div className="space-y-3 px-3 pb-6 pt-2 sm:px-4" data-nav-list>
      <CatchUpSection catchUp={catchUp} showHost={showHost} />
      <InboxApprovals showHost={showHost} />
      {rows.map((r) => (
        <div key={r.key}>{r.node}</div>
      ))}
    </div>
  );
}

function EntryCard({ entry, showHost }: { entry: RetainedEntry; showHost: boolean }) {
  return <InteractionCard item={entry.item} showHost={showHost} leaving={entry.mode === 'leaving'} />;
}
