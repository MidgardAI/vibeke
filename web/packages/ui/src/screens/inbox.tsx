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
import { useFrozenRows } from '../app/stable-list';
import { InboxRetainer, itemKey, type RetainedEntry } from '../lib/retain';
import { useCatchUp } from '../lib/use-catch-up';
import { InboxApprovals } from './approve';

/**
 * `after` (the menu-bar popover's agent list) renders below the cards, inside the same keyboard
 * list; with it, an empty inbox shows only `after` instead of the empty state.
 */
export function InboxScreen({ after }: { after?: ReactNode } = {}) {
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
  const built: { key: string; members: string[]; node: ReactNode }[] = [];
  for (const e of entries) {
    const b = e.mode === 'open' ? batchOf.get(e.key) : undefined;
    if (b) {
      if (rendered.has(b)) continue;
      rendered.add(b);
      built.push({ key: `batch:${b.fingerprint}:${b.host_id}`, members: b.items.map(itemKey), node: <BatchCard batch={b} showHost={showHost} /> });
      continue;
    }
    built.push({ key: e.key, members: [e.key], node: <EntryCard entry={e} showHost={showHost} /> });
  }
  // Cards must not move under a finger: the order holds while the user touches the list.
  const { rows, held, release, listProps } = useFrozenRows(built);

  const empty = built.length === 0 && approvals === 0 && catchUp.cards.length === 0;
  if (empty && !after) {
    return <Empty icon={<InboxIcon className="size-10" />} title={t.inbox.empty} hint={t.inbox.emptyHint} />;
  }
  if (empty) return <div data-nav-list>{after}</div>;
  const cards = (
    <div className="space-y-3 px-3 pb-6 pt-2 sm:px-4" data-nav-list={after ? undefined : true} {...listProps}>
      <CatchUpSection catchUp={catchUp} showHost={showHost} />
      <InboxApprovals showHost={showHost} />
      {held > 0 && (
        <div className="sticky top-2 z-10 flex justify-center">
          <button
            type="button"
            data-act="show-new"
            className="rounded-full bg-accent px-3 py-1 text-xs font-medium text-accent-fg shadow-md focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent"
            onClick={release}
          >
            {t.inbox.newItems(held)}
          </button>
        </div>
      )}
      {rows.map((r) => (
        <div key={r.key} data-row-key={r.key}>
          {r.node}
        </div>
      ))}
    </div>
  );
  if (!after) return cards;
  return (
    <div data-nav-list>
      <section aria-label={t.quick.approvals} className="border-b border-border">
        {cards}
      </section>
      {after}
    </div>
  );
}

function EntryCard({ entry, showHost }: { entry: RetainedEntry; showHost: boolean }) {
  return <InteractionCard item={entry.item} showHost={showHost} leaving={entry.mode === 'leaving'} />;
}
