// The workspace centre (spec 16 §9.1): a title bar (title, muted repo, ⋯; Preview, Share, Hand
// off, panel toggle), the tab strip (agents, their terminals, shells, previews) and the selected
// tab — an agent's conversation with the composer, a terminal mirror, or a preview. An agent
// opens on its conversation; panes without an agent open on the terminal.
// `locked` (a popped-out pane window): only the bound pane's tabs, no sidebar or panel controls,
// and switching tabs stays inside the window.

import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { MonitorPlay, MoreHorizontal, OctagonX, PanelRight, Pencil, PictureInPicture2, Plus, Search, Send, Server, Share2, Trash2 } from 'lucide-react';
import { groupBatches, hostKind, type InboxItem } from '@vibeke/core';
import { useApp, useHost, useHosts, useInboxItems, usePrefs } from '../../app/hooks';
import { emitUi, isMacLike } from '../../app/keyboard';
import { effectivePanel, layoutModeFor, rememberTab, selectedPane, togglePanelRoute, useWorkspaceRows } from '../../app/selection';
import { MenuButton as SidebarButton, useMediaQuery, useWide } from '../../app/shell';
import { useSurface } from '../../app/surface';
import { BatchCard } from '../../components/batch-card';
import { InteractionCard } from '../../components/interaction-card';
import { NewSheet } from '../../components/new-sheet';
import { Button, Empty, IconButton, Notice, Sheet, Spinner, TextField, cx } from '../../components/ui';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { keyLabel } from '../../lib/shortcuts';
import { noteUnsupported, supported } from '../../lib/supports';
import type { PaneRow } from '../../lib/tree';
import { runKey } from '../../lib/tree';
import type { WorkspaceRow } from '../../lib/workspaces';
import { navigate, type WorkspaceRoute } from '../../router';
import { HandoffSheet } from '../handoff';
import { usePaneActions } from '../pane/actions';
import { ActionBelt, type BeltTab } from '../pane/belt';
import { Composer } from '../pane/composer';
import { ShareSheet } from '../share';
import { Conversation } from './conversation';
import { showsCentreDiff } from './panel';
import { MenuButton, type MenuItem } from './menu';
import { PreviewTab } from './preview-tab';
import { TabStrip, workspacePreviews, workspaceTabs, type WsTab } from './tab-strip';
import { TerminalTab } from './terminal-tab';

const LazyCentreDiff = lazy(() => import('./panel/centre-diff'));

export function WorkspaceScreen({ route, locked = false }: { route: WorkspaceRoute; locked?: boolean }) {
  const rows = useWorkspaceRows();
  const host = useHost(route.host);
  const row = rows.find((r) => r.host === route.host && r.workspace.id === route.workspace);
  const pane = selectedPane(route, row);

  useEffect(() => {
    if (!locked && row && pane && row.panes.some((p) => p.pane.id === pane)) rememberTab(route.host, route.workspace, pane);
  }, [row?.key, pane]);

  if (!row) {
    if (!host || !host.dashboard)
      return (
        <Centre>
          <div className="flex flex-1 items-center justify-center">
            <Spinner />
          </div>
        </Centre>
      );
    return (
      <Centre title={t.workspace.notFound}>
        <Empty icon={<Server />} title={t.workspace.notFound} hint={t.workspace.notFoundHint} action={<Button onClick={() => navigate({ name: 'inbox' })}>{t.tabs.inbox}</Button>} />
      </Centre>
    );
  }
  const current = row.panes.find((p) => p.pane.id === pane);
  if (!current) {
    return (
      <Centre title={row.title} sub={row.hostName}>
        <Empty
          title={t.workspace.noPanes}
          action={
            <Button variant="primary" icon={<Plus />} onClick={() => emitUi('new-agent')}>
              {t.panes.newAgent}
            </Button>
          }
        />
      </Centre>
    );
  }
  return <Workspace route={route} row={row} current={current} locked={locked} />;
}

/** The tab the route selects (`show` = `term` / `preview:<id>`; else the pane's default). */
export function currentTabId(current: PaneRow, show: string | null | undefined): string {
  if (show?.startsWith('preview:')) return `p:${show.slice('preview:'.length)}`;
  if (show === 'term' || !current.run) return `t:${current.pane.id}`;
  return `a:${current.pane.id}`;
}

function Workspace({ route, row, current, locked }: { route: WorkspaceRoute; row: WorkspaceRow; current: PaneRow; locked: boolean }) {
  const app = useApp();
  const prefs = usePrefs();
  const host = useHost(row.host);
  const wide = useWide();
  const narrow = !useMediaQuery('(min-width: 960px)');
  const surface = useSurface();
  const mac = isMacLike(app.platform.mac);
  const [localShow, setLocalShow] = useState<string | null>(route.show ?? null);
  const show = locked ? localShow : (route.show ?? null);
  const scope = host?.info?.scope ?? host?.record.scope ?? 'view';
  const online = host?.status === 'online';
  const full = scope === 'full' && online;
  const previews = useMemo(() => workspacePreviews(row, host?.dashboard?.previews), [row, host?.dashboard?.previews]);
  const allTabs = useMemo(() => workspaceTabs(row, previews), [row, previews]);
  const tabs = locked ? allTabs.filter((x) => x.pane?.pane.id === current.pane.id && x.kind !== 'preview') : allTabs;
  const tabId = currentTabId(current, show);
  const tab: WsTab | null = allTabs.find((x) => x.id === tabId) ?? null;
  const [findOpen, setFindOpen] = useState(false);
  const [sheet, setSheet] = useState<null | 'new' | 'share' | 'handoff' | 'rename-pane' | 'close-pane' | 'rename-tab' | 'close-tab'>(null);

  const select = useCallback(
    (x: WsTab) => {
      const s = x.kind === 'preview' ? `preview:${x.preview!.id}` : x.kind === 'term' && x.pane?.run ? 'term' : null;
      if (locked) return setLocalShow(s);
      navigate({ ...route, pane: x.pane?.pane.id ?? route.pane, show: s, view: null }, { replace: true });
    },
    [locked, route],
  );
  const openTerminal = () => select({ id: `t:${current.pane.id}`, kind: 'term', pane: current, preview: null, label: '', title: '', secondary: true, status: null });

  const panelMode = layoutModeFor(typeof window === 'undefined' ? 1440 : window.innerWidth);
  const panelOpen = !locked && !!effectivePanel(route, prefs, panelMode);
  const togglePanel = () => {
    const next = togglePanelRoute(route, app.prefs.get(), layoutModeFor(window.innerWidth), 'changes');
    if (next.panelOpen !== undefined) app.prefs.patch({ panelOpen: next.panelOpen });
    navigate(next.route, { replace: true });
  };

  const repo = row.workspace.root_path.split('/').filter(Boolean).pop() ?? '';
  const multiHost = useHosts().length > 1;
  const sub = narrow ? [repo, row.hostName].filter(Boolean).join(' · ') : [repo !== row.title ? repo : null, multiHost ? row.hostName : null].filter(Boolean).join(' · ');
  const canShare = full && !!host && hostKind(host.record) === 'device' && !locked;
  const popOut = surface === 'full' ? app.platform.windows?.popOutPane : undefined;
  const conn = app.conn(row.host);

  const act = async (f: () => Promise<unknown>, method?: string) => {
    try {
      await f();
      app.haptic('success');
      void conn?.refresh().catch(() => {});
    } catch (e) {
      if (!method || !noteUnsupported(row.host, method, e)) app.toast(errorMessage(e), 'error');
    }
  };

  const hostTab = tab?.kind !== 'preview' ? current.tab : undefined;
  const tabMenu =
    full && hostTab && supported(row.host, 'tab.rename')
      ? {
          rename: () => setSheet('rename-tab'),
          close: () => setSheet('close-tab'),
          focus: () => void act(() => conn!.request('tab.focus', { tab: hostTab.id }), 'tab.focus'),
        }
      : null;

  const newTerminal = async () => {
    if (!conn) return;
    try {
      const r = await conn.request('tab.create', { workspace: row.workspace.id });
      void conn.refresh().catch(() => {});
      navigate({ ...route, pane: r.root_pane.id, show: null, view: null });
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    }
  };

  const wsMenu: (MenuItem | 'sep' | false)[] = [
    { label: t.tabs2.renamePane, icon: <Pencil />, disabled: !full, onSelect: () => setSheet('rename-pane') },
    tab?.kind === 'term' && { label: t.tabs2.findInTerminal, icon: <Search />, onSelect: () => setFindOpen(true) },
    !!popOut && { label: t.palette.popOut, icon: <PictureInPicture2 />, onSelect: () => popOut(row.host, current.pane.id) },
    narrow && canShare && { label: t.tabs2.share, icon: <Share2 />, onSelect: () => setSheet('share') },
    narrow && canShare && { label: t.tabs2.handoff, icon: <Send />, onSelect: () => setSheet('handoff') },
    'sep',
    { label: t.tabs2.closePane, icon: <Trash2 />, tone: 'danger', disabled: !full, onSelect: () => setSheet('close-pane') },
  ];

  const titleBlock = (
    <div className={cx('flex min-w-0 pl-0.5', narrow ? 'flex-1 flex-col leading-tight' : 'items-baseline gap-2')}>
      <h1 className={cx('truncate font-semibold', narrow ? 'text-[15px]' : 'text-[14px]')}>{row.title}</h1>
      {sub && <span className={cx('truncate text-muted', narrow ? 'text-xs' : 'text-[13px]')}>{sub}</span>}
    </div>
  );
  const previewButton = previews.length > 0 && !locked && (
    <IconButton label={t.tabs2.preview} active={tab?.kind === 'preview'} onClick={() => select(allTabs.find((x) => x.kind === 'preview')!)}>
      <MonitorPlay />
    </IconButton>
  );

  return (
    <div className="flex h-full min-h-0 flex-col bg-bg pt-safe">
      <header className={cx('titlebar flex h-11 shrink-0 items-center gap-1 border-b border-border pl-3 pr-2', !wide && 'titlebar-inset pl-1.5', narrow && 'h-12')}>
        {!wide && !locked && <SidebarButton />}
        {titleBlock}
        <MenuButton label={t.tabs2.workspaceMenu} icon={<MoreHorizontal />} align={narrow ? 'right' : 'left'} items={wsMenu} />
        {!narrow && <span className="flex-1" />}
        {tab?.kind === 'term' && (
          <IconButton label={t.pane.find} data-find aria-keyshortcuts="/" onClick={() => setFindOpen(true)} className={cx(narrow && 'hidden')}>
            <Search />
          </IconButton>
        )}
        {previewButton}
        {!narrow && canShare && (
          <>
            <Button size="sm" variant="ghost" icon={<Share2 />} onClick={() => setSheet('share')} className="text-muted hover:text-fg">
              {t.tabs2.share}
            </Button>
            <Button size="sm" variant="ghost" icon={<Send />} onClick={() => setSheet('handoff')} className="text-muted hover:text-fg">
              {t.tabs2.handoff}
            </Button>
          </>
        )}
        {!locked && (
          <IconButton label={`${t.sidebar.togglePanel} (${keyLabel(mac, 'mod+3')})`} active={panelOpen} onClick={togglePanel}>
            <PanelRight />
          </IconButton>
        )}
      </header>
      <TabStrip
        tabs={tabs}
        current={tabId}
        onSelect={select}
        onNewAgent={full ? () => setSheet('new') : undefined}
        onNewTerminal={full ? () => void newTerminal() : undefined}
        tabMenu={tabMenu}
        locked={locked}
      />
      {!locked && showsCentreDiff(route) ? (
        <Suspense
          fallback={
            <div className="flex flex-1 items-center justify-center">
              <Spinner />
            </div>
          }
        >
          <LazyCentreDiff route={route} />
        </Suspense>
      ) : tab?.kind === 'preview' || tabId.startsWith('p:') ? (
        <PreviewTab hostId={row.host} preview={tab?.preview ?? null} />
      ) : (
        <PaneBody
          key={`${row.host}/${current.pane.id}`}
          hostId={row.host}
          row={current}
          mode={tab?.kind === 'agent' ? 'agent' : 'term'}
          findOpen={findOpen}
          setFindOpen={setFindOpen}
          onOpenTerminal={openTerminal}
        />
      )}

      <NewSheet open={sheet === 'new'} onClose={() => setSheet(null)} hostId={row.host} workspaceId={row.workspace.id} />
      <ShareSheet row={current} open={sheet === 'share'} onClose={() => setSheet(null)} />
      <HandoffSheet row={current} open={sheet === 'handoff'} onClose={() => setSheet(null)} />
      <PromptSheet
        open={sheet === 'rename-pane'}
        title={t.tabs2.renamePane.replace('…', '')}
        label={t.panes.renamePrompt}
        initial={current.pane.title ?? ''}
        onClose={() => setSheet(null)}
        onSubmit={(v) => void act(() => conn!.request('pane.rename', { pane: current.pane.id, title: v || null }))}
      />
      <ConfirmSheet
        open={sheet === 'close-pane'}
        title={t.tabs2.closePane}
        body={t.tabs2.closePaneConfirm}
        confirm={t.tabs2.closePane}
        onClose={() => setSheet(null)}
        onConfirm={() => void act(() => conn!.request('pane.close', { pane: current.pane.id }))}
      />
      {hostTab && (
        <>
          <PromptSheet
            open={sheet === 'rename-tab'}
            title={t.tabs2.renameTab}
            label={t.tabs2.renamePrompt}
            initial={hostTab.title ?? ''}
            onClose={() => setSheet(null)}
            onSubmit={(v) => void act(() => conn!.request('tab.rename', { tab: hostTab.id, title: v || null }), 'tab.rename')}
          />
          <ConfirmSheet
            open={sheet === 'close-tab'}
            title={t.tabs2.closeTab}
            body={t.tabs2.closeTabConfirm}
            confirm={t.tabs2.closeTab}
            onClose={() => setSheet(null)}
            onConfirm={() => void act(() => conn!.request('tab.close', { tab: hostTab.id }), 'tab.close')}
          />
        </>
      )}
    </div>
  );
}

// ---- a pane: conversation or terminal, approvals, composer ----------------------------------

function PaneBody({
  hostId,
  row,
  mode,
  findOpen,
  setFindOpen,
  onOpenTerminal,
}: {
  hostId: string;
  row: PaneRow;
  mode: 'agent' | 'term';
  findOpen: boolean;
  setFindOpen(v: boolean): void;
  onOpenTerminal(): void;
}) {
  const app = useApp();
  const host = useHost(hostId);
  const inbox = useInboxItems();
  const run = row.run;
  const scope = host?.info?.scope ?? host?.record.scope ?? 'view';
  const online = host?.status === 'online';
  const canType = scope === 'full' && online;
  const working = run?.execution.value === 'working' || run?.execution.value === 'starting';
  const [refreshKey, setRefreshKey] = useState(0);
  const burstRef = useRef<(() => void) | null>(null);
  const onSent = useCallback(() => {
    burstRef.current?.();
    setRefreshKey((k) => k + 1);
  }, []);
  const actions = usePaneActions(hostId, row.pane.id, run, scope, onSent);
  const [text, setText] = useState('');
  const [belt, setBelt] = useState<BeltTab | null>(null);
  const [noEcho, setNoEcho] = useState(false);
  const cards = inbox.filter((it) => it.host_id === hostId && it.interaction.pane === row.pane.id);

  // Viewing a finished run marks it seen.
  useEffect(() => {
    if (run) app.prefs.markSeen(runKey(hostId, run.id), run.done_rev);
  }, [run?.id, run?.done_rev]);

  const approvals = cards.length > 0 ? <Approvals items={cards} /> : null;

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {mode === 'agent' && run ? (
        <Conversation hostId={hostId} pane={row.pane.id} run={run} cwd={run.cwd ?? row.pane.cwd} refreshKey={refreshKey} onOpenTerminal={onOpenTerminal} tail={approvals} />
      ) : (
        <>
          <TerminalTab hostId={hostId} pane={row.pane.id} working={working} findOpen={findOpen} setFindOpen={setFindOpen} onNoEcho={setNoEcho} burstRef={burstRef} />
          {approvals && <div className="max-h-[45vh] shrink-0 space-y-2 overflow-y-auto border-t border-border px-3 py-2">{approvals}</div>}
        </>
      )}
      <div className="shrink-0 pb-safe">
        <div className="mx-auto w-full max-w-[780px] space-y-1 px-3 empty:hidden sm:px-4">
          {mode === 'term' && noEcho && canType && <Notice tone="warn">{t.composer.password}</Notice>}
          {!online && <Notice tone="warn">{t.composer.offline}</Notice>}
          {online && scope === 'view' && <Notice>{t.composer.readOnly}</Notice>}
          {online && scope === 'approve' && (
            <Notice
              action={
                run && working ? (
                  <Button size="sm" variant="outline" icon={<OctagonX />} onClick={() => void actions.interrupt()}>
                    {t.pane.interrupt}
                  </Button>
                ) : undefined
              }
            >
              {t.composer.approveOnly}
            </Notice>
          )}
        </div>
        {canType && (
          <Composer
            hostId={hostId}
            actions={actions}
            text={text}
            setText={setText}
            isAgent={!!run && mode === 'agent'}
            sttAvailable={host?.info?.features.includes('stt') ?? false}
            run={run}
            working={working}
            more={
              <ActionBelt
                tab={belt ?? 'keys'}
                setTab={setBelt}
                actions={actions}
                harness={run?.harness ?? null}
                canType={canType}
                onInsert={(s) => setText((cur) => (cur ? `${cur} ${s}` : s))}
                zen={false}
                setZen={() => {}}
              />
            }
          />
        )}
      </div>
    </div>
  );
}

/** Open approvals of the pane: one compact card each, or a batch card for identical requests. */
function Approvals({ items }: { items: InboxItem[] }) {
  const batches = groupBatches(items);
  const inBatch = new Set(batches.flatMap((b) => b.items.map((i) => i.interaction.id)));
  return (
    <div className="space-y-2" data-nav-list>
      {batches.map((b) => (
        <BatchCard key={b.fingerprint} batch={b} showHost={false} variant="compact" />
      ))}
      {items
        .filter((i) => !inBatch.has(i.interaction.id))
        .map((c) => (
          <InteractionCard key={c.interaction.id} item={c} variant="compact" />
        ))}
    </div>
  );
}

// ---- small sheets ----------------------------------------------------------------------------

function PromptSheet({ open, title, label, initial, onClose, onSubmit }: { open: boolean; title: string; label: string; initial: string; onClose(): void; onSubmit(v: string): void }) {
  const [v, setV] = useState(initial);
  useEffect(() => {
    if (open) setV(initial);
  }, [open]);
  return (
    <Sheet open={open} onClose={onClose} title={title}>
      <form
        className="space-y-3"
        onSubmit={(e) => {
          e.preventDefault();
          onClose();
          onSubmit(v.trim());
        }}
      >
        <TextField label={label} value={v} autoFocus onChange={(e) => setV(e.target.value)} />
        <Button variant="primary" block type="submit">
          {t.save}
        </Button>
      </form>
    </Sheet>
  );
}

function ConfirmSheet({ open, title, body, confirm, onClose, onConfirm }: { open: boolean; title: string; body: string; confirm: string; onClose(): void; onConfirm(): void }) {
  return (
    <Sheet open={open} onClose={onClose} title={title} role="alertdialog">
      <p className="text-sm text-muted">{body}</p>
      <div className="mt-4 flex gap-2">
        <Button className="flex-1" variant="outline" onClick={onClose}>
          {t.cancel}
        </Button>
        <Button
          className="flex-1"
          variant="danger"
          onClick={() => {
            onClose();
            onConfirm();
          }}
        >
          {confirm}
        </Button>
      </div>
    </Sheet>
  );
}

function Centre({ title, sub, children }: { title?: string; sub?: string; children: ReactNode }) {
  const wide = useWide();
  return (
    <div className="flex h-full min-h-0 flex-col pt-safe">
      <header className={cx('titlebar flex h-11 shrink-0 items-center gap-2 border-b border-border px-3', !wide && 'titlebar-inset pl-1.5')}>
        {!wide && <SidebarButton />}
        <div className="flex min-w-0 flex-1 items-baseline gap-2">
          {title && <h1 className="truncate text-[14px] font-semibold">{title}</h1>}
          {sub && <span className="truncate text-[13px] text-muted">{sub}</span>}
        </div>
      </header>
      {children}
    </div>
  );
}

