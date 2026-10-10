// The menu-bar popover's agent list (spec 16 §16.2): every agent on every connected host, the
// ones that need you first, with its state and how long it has been in it. A row opens the
// agent's pane in the main window.

import type { Attention } from '@vibeke/core';
import { useApp, useNow } from '../app/hooks';
import { stateTone, stateWord } from '../components/pane-row';
import { Dot, Row, StatusDot } from '../components/ui';
import { t } from '../i18n';
import { shortDuration } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { rowTitle, workspaceLabel, type PaneRow, type PaneTree } from '../lib/tree';
import { formatRoute, workspaceRoute } from '../router';

const RANK: Record<Attention, number> = { interaction: 0, needs_input: 1, working: 2, idle: 3 };

/** Agent rows of one host, most urgent first, then the most recent state change. */
export function agentRows(rows: readonly PaneRow[]): PaneRow[] {
  return rows
    .filter((r) => r.run)
    .sort((a, b) => RANK[a.attention] - RANK[b.attention] || b.run!.execution.since_ms - a.run!.execution.since_ms || a.key.localeCompare(b.key));
}

export function QuickAgents({ tree }: { tree: PaneTree }) {
  const app = useApp();
  const now = useNow(5000);
  const groups = tree.hosts.map((g) => ({ g, rows: agentRows(g.rows) }));
  // Host headings only help when there is more than one host, or one is not connected.
  const headings = tree.hosts.length > 1 || tree.hosts.some((g) => g.host.status !== 'online');
  const total = groups.reduce((n, x) => n + x.rows.length, 0);

  const open = (r: PaneRow) => {
    app.platform.windows?.openMain?.(formatRoute(workspaceRoute(r.host, r.pane.workspace, { pane: r.pane.id })));
    app.platform.windows?.close?.();
  };

  return (
    <section aria-label={t.quick.agents} className="px-2 pb-3 pt-2" data-quick-agents>
      <h2 className="px-2 pb-1 text-2xs font-medium uppercase tracking-wide text-faint">{t.quick.agents}</h2>
      {tree.hosts.length === 0 && <p className="px-2 py-2 text-sm text-muted">{t.quick.noHosts}</p>}
      {groups.map(({ g, rows }) => {
        const id = g.host.record.host_id;
        const online = g.host.status === 'online';
        const name = g.host.info?.host_name ?? g.host.record.name;
        return (
          <div key={id} className="pb-1" data-quick-host={id}>
            {headings && (
              <div className="flex items-center gap-2 px-2 pb-0.5 pt-2 text-xs text-muted">
                {!online && <StatusDot status="offline" />}
                <span className="min-w-0 flex-1 truncate font-medium">{name}</span>
                {!online && <span className="shrink-0 text-faint">{t.quick.hostState[g.host.status] ?? t.quick.hostProblem}</span>}
              </div>
            )}
            {rows.map((r) => (
              <Row
                key={r.key}
                data-nav-item={`agent:${r.key}`}
                data-attention={r.attention}
                leading={<Dot tone={stateTone(r)} />}
                trailing={
                  <>
                    <span data-tone={stateTone(r)}>{stateWord(r)}</span>
                    <span className="tabular-nums">{shortDuration(now - r.run!.execution.since_ms)}</span>
                  </>
                }
                sub={[workspaceLabel(r.workspace), harnessLabel(r.run!.harness)].filter(Boolean).join(' · ')}
                onClick={() => open(r)}
              >
                {rowTitle(r)}
              </Row>
            ))}
          </div>
        );
      })}
      {tree.hosts.length > 0 && total === 0 && (
        <div className="px-2 py-2">
          <p className="text-sm text-muted">{t.quick.noAgents}</p>
          <p className="text-xs text-faint">{t.quick.noAgentsHint}</p>
        </div>
      )}
    </section>
  );
}
