import { useState } from 'react';
import { ChevronRight, Plus, Server } from 'lucide-react';
import { displayName } from '@vibeke/core';
import { useHosts, useTree } from '../app/hooks';
import { NewSheet } from '../components/new-sheet';
import { PaneRowView } from '../components/pane-row';
import { Button, Dot, Empty, SectionLabel, cx } from '../components/ui';
import { t } from '../i18n';
import { navigate } from '../router';

export function PanesScreen() {
  const tree = useTree();
  const hosts = useHosts();
  const [newOpen, setNewOpen] = useState(false);
  const multi = hosts.length > 1;

  if (hosts.length === 0) {
    return (
      <Empty
        icon={<Server className="size-10" />}
        title={t.panes.noHosts}
        hint={t.panes.pairFirst}
        action={<Button variant="primary" onClick={() => navigate({ name: 'pair', d: null })}>{t.crew.pair}</Button>}
      />
    );
  }

  const jumpFirst = () => {
    const first = tree.needYou[0];
    if (first) document.getElementById(`row-${first.key}`)?.scrollIntoView({ block: 'center', behavior: 'smooth' });
  };

  return (
    <div className="pb-4" data-nav-list>
      <div className="flex items-center gap-2 px-4 pt-3">
        {tree.needYou.length > 0 ? (
          <button type="button" onClick={jumpFirst} className="flex h-9 items-center gap-2 rounded-full bg-need px-3 text-sm font-medium">
            <Dot tone="need" />
            {t.panes.needYou(tree.needYou.length)}
            <ChevronRight className="size-4 text-muted" />
          </button>
        ) : (
          <span className="text-sm text-muted">{t.focus.empty}</span>
        )}
        <span className="flex-1" />
        <Button size="sm" variant="secondary" icon={<Plus className="size-4" />} onClick={() => setNewOpen(true)}>
          {t.panes.newAgent}
        </Button>
      </div>

      {tree.pinned.length > 0 && (
        <section>
          <SectionLabel>{t.panes.pinned}</SectionLabel>
          <div className="inset-group divide-y divide-border border-y border-border bg-surface">
            {tree.pinned.map((r) => (
              <PaneRowView key={r.key} row={r} showHost={multi} />
            ))}
          </div>
        </section>
      )}

      {tree.hosts.map((g) => (
        <section key={g.host.record.host_id}>
          {multi && (
            <div className="flex items-center gap-2 px-4 pb-1 pt-5 text-sm font-semibold sm:px-8">
              <Dot tone={g.host.status === 'online' ? 'ok' : g.host.status === 'connecting' ? 'warn' : 'danger'} />
              {g.host.info?.host_name ?? g.host.record.name}
              {g.needsYou > 0 && <span className="text-xs font-normal text-muted">· {t.crew.needYou(g.needsYou)}</span>}
            </div>
          )}
          {!g.host.dashboard && <div className="px-4 py-3 text-sm text-muted sm:px-8">{g.host.status === 'online' ? t.loading : t.conn.hostOffline}</div>}
          {g.host.dashboard && g.rows.length === 0 && <div className="px-4 py-3 text-sm text-muted sm:px-8">{t.panes.empty}</div>}
          {g.workspaces.map((w) =>
            w.tabs.length === 0 ? null : (
              <div key={w.workspace.id}>
                <SectionLabel right={w.needsYou > 0 ? <Dot tone="need" /> : undefined}>
                  {displayName(w.workspace)}
                  {w.workspace.branch && <span className="ml-1.5 normal-case tracking-normal text-faint">⎇ {w.workspace.branch}</span>}
                </SectionLabel>
                <div className="inset-group border-y border-border bg-surface">
                  {w.tabs.map((tg, i) => (
                    <div key={tg.tab.id} className={cx(i > 0 && 'border-t-4 border-bg')}>
                      {w.tabs.length > 1 && (
                        <div className="px-4 pt-1.5 text-2xs text-faint">
                          {tg.tab.number}. {tg.tab.title ?? ''}
                        </div>
                      )}
                      <div className="divide-y divide-border">
                        {tg.rows.map((r) => (
                          <PaneRowView key={r.key} row={r} showHost={false} />
                        ))}
                      </div>
                    </div>
                  ))}
                </div>
              </div>
            ),
          )}
        </section>
      ))}
      <NewSheet open={newOpen} onClose={() => setNewOpen(false)} />
    </div>
  );
}

export function FocusScreen() {
  const tree = useTree();
  const hosts = useHosts();
  if (tree.needYou.length === 0) return <Empty title={t.focus.empty} hint={t.focus.emptyHint} />;
  return (
    <div className="inset-group divide-y divide-border border-y border-border bg-surface mt-3" data-nav-list>
      {tree.needYou.map((r) => (
        <PaneRowView key={r.key} row={r} showHost={hosts.length > 1} />
      ))}
    </div>
  );
}
