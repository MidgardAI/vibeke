// Workspace sidebar: top actions (New agent, Inbox, Search), pinned workspaces, then every
// workspace grouped by what it asks of the user, with an inline filter and host / done filters.
// Inline on wide windows, a drawer on narrow ones. Rows are `data-nav-item`s, so j/k walk them
// whenever the centre has no list of its own.

import { useEffect, useRef, useState, type ReactNode } from 'react';
import {
  Check,
  Circle,
  CircleAlert,
  CircleCheck,
  CircleDashed,
  CircleHelp,
  GitBranch,
  Home,
  Inbox,
  ListFilter,
  PackageOpen,
  PanelLeft,
  PanelRight,
  Pin,
  PinOff,
  Plus,
  Search,
  Settings,
  X,
} from 'lucide-react';
import { Badge, HarnessIcon, IconButton, Kbd, RelTime, Row, SectionHeader, Sheet, SheetRow, StatusDot, cx, type Status } from '../components/ui';
import { t } from '../i18n';
import { keyLabel } from '../lib/shortcuts';
import type { WorkspaceGroupId, WorkspaceRow } from '../lib/workspaces';
import { navigate, workspaceRoute, type Route } from '../router';
import { useApprovalCount } from './approval-stores';
import { useIncoming, useIncomingCount } from './handoff-stores';
import { useApp, useHosts, useInboxItems, usePrefs } from './hooks';
import { emitUi, isMacLike } from './keyboard';
import { drawerOpen, useWorkspaces, workspacePinKey } from './selection';

const GROUP_ICON: Record<WorkspaceGroupId, ReactNode> = {
  needs: <CircleAlert className="size-3.5 text-need" strokeWidth={2} />,
  review: <CircleCheck className="size-3.5 text-add" strokeWidth={2} />,
  working: <CircleDashed className="size-3.5 text-info" strokeWidth={2} />,
  done: <CircleCheck className="size-3.5 text-faint" strokeWidth={2} />,
  idle: <Circle className="size-3.5 text-faint" strokeWidth={2} />,
};

function rowStatus(r: WorkspaceRow): Status | null {
  if (r.group === 'needs') return r.panes.some((p) => p.run?.execution.value === 'error') && !r.open ? 'error' : 'need';
  if (r.group === 'working') return 'working';
  if (r.unread) return 'review';
  return null;
}

export function Sidebar({ route, mode }: { route: Route; mode: 'inline' | 'drawer' }) {
  const app = useApp();
  const prefs = usePrefs();
  const hosts = useHosts();
  const items = useInboxItems();
  const incoming = useIncoming();
  const handoffs = useIncomingCount();
  const approvals = useApprovalCount();
  const anyHandoffs = [...incoming.values()].some((h) => h.list.length > 0);
  const mac = isMacLike(app.platform.mac);
  const [query, setQuery] = useState('');
  const [filtering, setFiltering] = useState(false);
  const [options, setOptions] = useState(false);
  const [menuFor, setMenuFor] = useState<WorkspaceRow | null>(null);
  const list = useWorkspaces(query);
  const multiHost = hosts.length > 1;
  const collapsed = new Set(prefs.collapsed);
  const inWorkspace = route.name === 'workspace';

  const close = () => mode === 'drawer' && drawerOpen.set(false);
  const go = (r: Route) => {
    close();
    navigate(r);
  };
  const open = (r: WorkspaceRow) => go(workspaceRoute(r.host, r.workspace.id));
  const isActive = (r: WorkspaceRow) => route.name === 'workspace' && route.host === r.host && route.workspace === r.workspace.id;

  const rowView = (r: WorkspaceRow) => {
    const status = rowStatus(r);
    const sub = [
      r.branch ? (
        <span key="b" className="flex min-w-0 items-center gap-1">
          <GitBranch className="size-3 shrink-0" />
          <span className="truncate">{r.branch}</span>
        </span>
      ) : null,
      multiHost ? (
        <span key="h" className="truncate">
          {r.hostName}
        </span>
      ) : null,
      r.checkLabel ? (
        <span key="c" className={cx('flex shrink-0 items-center gap-1', r.group === 'review' && 'text-add')}>
          {r.group === 'review' && <Check className="size-3" />}
          {r.checkLabel}
        </span>
      ) : null,
    ].filter(Boolean);
    if (!r.branch && r.summary)
      sub.unshift(
        <span key="s" className="truncate">
          {r.summary}
        </span>,
      );
    return (
      <Row
        key={r.key}
        data-nav-item={r.key}
        active={isActive(r)}
        onClick={() => open(r)}
        onContextMenu={(e) => {
          e.preventDefault();
          setMenuFor(r);
        }}
        title={r.title}
        leading={
          <span className="relative flex">
            <HarnessIcon harness={r.harness} />
            {status && <StatusDot status={status} ring className="absolute -bottom-1 -right-1 [--ring:var(--sidebar-bg)]" />}
          </span>
        }
        trailing={r.open > 0 ? <Badge n={r.open} /> : <RelTime ms={r.lastActivityMs} />}
        sub={
          sub.length ? (
            <>
              {sub.map((node, i) => (
                <span key={i} className="flex min-w-0 items-center gap-1">
                  {i > 0 && <span className="text-faint">·</span>}
                  {node}
                </span>
              ))}
            </>
          ) : undefined
        }
      >
        <span className={cx(r.unread || r.group === 'needs' ? 'font-medium text-fg' : '')}>{r.title}</span>
      </Row>
    );
  };

  const section = (id: string, title: string, icon: ReactNode, rows: WorkspaceRow[]) => (
    <section key={id} aria-label={title} className="pb-1">
      <SectionHeader title={title} icon={icon} count={rows.length} collapsed={collapsed.has(id)} onToggle={() => app.prefs.toggleCollapsed(id)} />
      {!collapsed.has(id) && <div className="space-y-px">{rows.map(rowView)}</div>}
    </section>
  );

  const total = list.all.length;
  const shown = list.pinned.length + list.groups.reduce((n, g) => n + g.rows.length, 0);
  const offline = hosts.filter((h) => h.status !== 'online');

  return (
    <nav aria-label={t.sidebar.label} className={cx('sidebar flex h-full shrink-0 select-none flex-col', mode === 'inline' ? 'w-[280px] border-r border-border' : 'sidebar-drawer w-full pt-safe')}>
      <div className="sidebar-top flex h-11 shrink-0 items-center justify-end gap-0.5 px-2">
        {mode === 'inline' ? (
          <IconButton label={`${t.sidebar.hide} (${keyLabel(mac, 'mod+\\')})`} onClick={() => app.prefs.patch({ sidebarHidden: true })}>
            <PanelLeft />
          </IconButton>
        ) : (
          <IconButton label={t.close} onClick={close}>
            <X />
          </IconButton>
        )}
      </div>

      <div className="shrink-0 space-y-px px-2 pb-2">
        <Row leading={<Plus className="size-4" />} onClick={() => (close(), emitUi('new-agent'))}>
          {t.sidebar.newAgent}
        </Row>
        <Row
          leading={<Inbox className="size-4" />}
          active={route.name === 'inbox' || route.name === 'interaction' || route.name === 'approve'}
          trailing={<Badge n={items.length + approvals} />}
          onClick={() => go({ name: 'inbox' })}
          title={`${t.sidebar.inbox} (${keyLabel(mac, 'mod+1')})`}
        >
          {t.sidebar.inbox}
        </Row>
        {(anyHandoffs || route.name === 'handoffs') && (
          <Row
            leading={<PackageOpen className="size-4" />}
            active={route.name === 'handoffs'}
            trailing={<Badge n={handoffs} />}
            onClick={() => go({ name: 'handoffs', host: null, id: null })}
          >
            {t.sidebar.handoffs}
          </Row>
        )}
        <Row leading={<Search className="size-4" />} trailing={<Kbd>{keyLabel(mac, 'mod+k')}</Kbd>} onClick={() => (close(), emitUi('palette'))}>
          {t.sidebar.search}
        </Row>
      </div>

      <div className="vk-scroll min-h-0 flex-1 overflow-y-auto border-t border-border px-2 pb-3 pt-2" data-nav-list="sidebar">
        {list.pinned.length > 0 && section('pinned', t.sidebar.pinned, <Pin className="size-3.5 text-faint" />, list.pinned)}

        <div className="group/ws relative flex h-7 items-center gap-0.5 pl-2 pr-0.5 text-xs font-medium text-muted">
          <span className="flex-1">{t.sidebar.workspaces}</span>
          <IconButton label={t.sidebar.filter} active={filtering || !!query} className="size-6" onClick={() => (filtering ? (setFiltering(false), setQuery('')) : setFiltering(true))}>
            <Search className="size-3.5!" />
          </IconButton>
          <IconButton label={t.sidebar.options} active={options || !!prefs.hostFilter || !prefs.showDone} className="size-6" onClick={() => setOptions((v) => !v)} aria-expanded={options}>
            <ListFilter className="size-3.5!" />
          </IconButton>
          {options && <FilterMenu onClose={() => setOptions(false)} onHosts={() => go({ name: 'crew' })} />}
        </div>
        {filtering && (
          <div className="px-1 pb-1.5">
            <input
              autoFocus
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === 'Escape') {
                  e.preventDefault();
                  e.stopPropagation();
                  setQuery('');
                  setFiltering(false);
                }
              }}
              placeholder={t.sidebar.filterPlaceholder}
              aria-label={t.sidebar.filter}
              className="h-7 w-full rounded-md border border-border bg-bg px-2 text-sm placeholder:text-faint focus:border-border-strong focus:outline-none"
            />
          </div>
        )}

        {list.groups.map((g) => section(g.id, t.sidebar.groups[g.id]!, GROUP_ICON[g.id], g.rows))}

        {hosts.length > 0 && total === 0 && hosts.some((h) => h.dashboard) && (
          <div className="px-2 py-3 text-xs text-muted">
            <div className="text-sm text-fg/80">{t.sidebar.empty}</div>
            {t.sidebar.emptyHint}
          </div>
        )}
        {total > 0 && shown === 0 && <div className="px-2 py-3 text-xs text-muted">{t.sidebar.noMatch}</div>}
        {list.hidden > 0 && shown > 0 && (
          <button type="button" className="vk-focus mt-1 rounded-md px-2 py-1 text-2xs text-faint hover:text-muted" onClick={() => (setQuery(''), app.prefs.patch({ hostFilter: null, showDone: true }))}>
            {t.sidebar.hidden(list.hidden)}
          </button>
        )}
      </div>

      {offline.length > 0 && (
        <div className="shrink-0 space-y-px border-t border-border px-2 py-1.5">
          {offline.map((h) => (
            <div key={h.record.host_id} className="flex h-6 items-center gap-2 px-2 text-xs text-muted">
              <StatusDot status={h.status === 'connecting' ? 'working' : 'offline'} />
              <span className="min-w-0 flex-1 truncate">{h.info?.host_name ?? h.record.name}</span>
              <span className="text-faint">{h.status === 'connecting' ? t.sidebar.connecting : t.sidebar.offline}</span>
              {h.status !== 'connecting' && (
                <button type="button" className="vk-focus rounded px-1 text-faint hover:text-fg" onClick={() => app.conn(h.record.host_id)?.reconnectNow()}>
                  {t.sidebar.reconnect}
                </button>
              )}
            </div>
          ))}
        </div>
      )}

      <div className="flex h-11 shrink-0 items-center gap-0.5 border-t border-border px-2 pb-safe">
        <button
          type="button"
          onClick={() => go({ name: 'pair', d: null })}
          className="vk-focus flex h-7 min-w-0 flex-1 items-center gap-2 rounded-md px-2 text-sm text-fg/90 hover:bg-hover"
        >
          <Plus className="size-4 text-muted" />
          <span className="truncate">{t.sidebar.pairHost}</span>
        </button>
        <IconButton label={`${t.sidebar.togglePanel} (${keyLabel(mac, 'mod+3')})`} disabled={!inWorkspace} onClick={() => (close(), emitUi('panel'))}>
          <PanelRight />
        </IconButton>
        <IconButton label={t.sidebar.home} onClick={() => go({ name: 'home' })}>
          <Home />
        </IconButton>
        <IconButton label={`${t.sidebar.help} (?)`} onClick={() => (close(), emitUi('shortcuts'))}>
          <CircleHelp />
        </IconButton>
        <IconButton label={`${t.sidebar.settings} (${keyLabel(mac, 'mod+4')})`} active={route.name === 'settings'} onClick={() => go({ name: 'settings' })}>
          <Settings />
        </IconButton>
      </div>

      {menuFor && <RowMenu row={menuFor} onClose={() => setMenuFor(null)} />}
    </nav>
  );
}

function RowMenu({ row, onClose }: { row: WorkspaceRow; onClose(): void }) {
  const app = useApp();
  const prefs = usePrefs();
  const key = workspacePinKey(row.host, row.workspace.id);
  const pinnedHere = prefs.pins.includes(key);
  const pinnedPanes = row.panes.filter((p) => p.pinned).map((p) => p.key);
  return (
    <Sheet open onClose={onClose} title={row.title}>
      <SheetRow
        icon={row.pinned ? <PinOff /> : <Pin />}
        onClick={() => {
          if (row.pinned) app.prefs.patch({ pins: prefs.pins.filter((k) => k !== key && !pinnedPanes.includes(k)) });
          else if (!pinnedHere) app.prefs.patch({ pins: [...prefs.pins, key] });
          onClose();
        }}
      >
        {row.pinned ? t.sidebar.unpin : t.sidebar.pin}
      </SheetRow>
    </Sheet>
  );
}

/** Host and done filters: a small non-modal menu under the header button. */
function FilterMenu({ onClose, onHosts }: { onClose(): void; onHosts(): void }) {
  const app = useApp();
  const prefs = usePrefs();
  const hosts = useHosts();
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const down = (e: PointerEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node) && !(e.target as Element).closest?.('[aria-expanded]')) onClose();
    };
    const key = (e: KeyboardEvent) => {
      if (e.key === 'Escape') {
        e.preventDefault();
        onClose();
      }
    };
    document.addEventListener('pointerdown', down, true);
    document.addEventListener('keydown', key, true);
    return () => {
      document.removeEventListener('pointerdown', down, true);
      document.removeEventListener('keydown', key, true);
    };
  }, [onClose]);
  const item = (label: ReactNode, checked: boolean, onClick: () => void, key?: string) => (
    <button
      key={key}
      type="button"
      role="menuitemcheckbox"
      aria-checked={checked}
      onClick={onClick}
      className="vk-focus flex h-7 w-full items-center gap-2 rounded-[5px] px-2 text-left text-sm text-fg/90 hover:bg-hover"
    >
      <span className="flex size-3.5 items-center justify-center">{checked && <Check className="size-3.5" />}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
    </button>
  );
  return (
    <div ref={ref} role="menu" aria-label={t.sidebar.options} className="animate-pop absolute right-0 top-7 z-30 w-56 rounded-lg bg-surface-2 p-1 text-fg shadow-[var(--shadow)]">
      <div className="px-2 pb-0.5 pt-1 text-2xs font-medium text-faint">{t.sidebar.hosts}</div>
      {item(t.sidebar.allHosts, !prefs.hostFilter, () => app.prefs.patch({ hostFilter: null }), 'all')}
      {hosts.map((h) =>
        item(
          <span className="flex items-center gap-2">
            <StatusDot status={h.status === 'online' ? 'review' : h.status === 'connecting' ? 'working' : 'offline'} />
            {h.info?.host_name ?? h.record.name}
          </span>,
          prefs.hostFilter === h.record.host_id,
          () => app.prefs.patch({ hostFilter: prefs.hostFilter === h.record.host_id ? null : h.record.host_id }),
          h.record.host_id,
        ),
      )}
      <div className="my-1 border-t border-border" />
      {item(t.sidebar.showDone, prefs.showDone, () => app.prefs.patch({ showDone: !prefs.showDone }))}
      <div className="my-1 border-t border-border" />
      <button type="button" role="menuitem" onClick={() => (onClose(), onHosts())} className="vk-focus flex h-7 w-full items-center rounded-[5px] px-2 pl-7.5 text-left text-sm text-muted hover:bg-hover hover:text-fg">
        {t.sidebar.manageHosts}
      </button>
    </div>
  );
}
