import { useComposerDraft } from '../../lib/composer-draft';
// The workspace centre (spec 16 §9.1): a title bar (title, muted repo, ⋯; Preview, Share, Hand
// off, panel toggle), the tab strip (agents, their terminals, shells, previews) and the selected
// tab — an agent's conversation with the composer, a terminal mirror (key belt, composer typing
// into the pane), or a preview. An agent opens on the workspace's agent view (conversation by
// default, or the agent's own terminal UI; lib/agent-view.ts); panes without an agent open on the
// terminal.
// `locked` (a popped-out pane window): only the bound pane's tabs, no sidebar or panel controls,
// and switching tabs stays inside the window.

import { Suspense, lazy, useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { ChevronDown, ChevronUp, MonitorPlay, MoreHorizontal, OctagonX, PanelRight, Pencil, PictureInPicture2, Plus, RotateCcw, Search, Send, Server, Share2, Trash2 } from 'lucide-react';
import { groupBatches, hostKind, type InboxItem } from '@vibeke/core';
import { useApp, useHost, useHosts, useInboxItems, usePrefs } from '../../app/hooks';
import { emitUi, isMacLike, onAgentViewRequest, type AgentViewRequest } from '../../app/keyboard';
import { effectivePanel, layoutModeFor, rememberTab, selectedPane, togglePanelRoute, useWorkspaceRows } from '../../app/selection';
import { MenuButton as SidebarButton, useMediaQuery, useWide } from '../../app/shell';
import { useSurface } from '../../app/surface';
import { BatchCard } from '../../components/batch-card';
import { InteractionCard } from '../../components/interaction-card';
import { NewSheet } from '../../components/new-sheet';
import { Button, Empty, IconButton, Notice, Sheet, Spinner, TextField, cx } from '../../components/ui';
import { t } from '../../i18n';
import { agentViewFor, hasViewOverride, otherView, paneTabId, showFor, tabBody, type AgentView } from '../../lib/agent-view';
import { errorMessage } from '../../lib/answer';
import { composerShowsStop } from '../../lib/guards';
import { TAB_PANEL_ID, tabDomId } from '../../lib/tabs-nav';
import { keyLabel } from '../../lib/shortcuts';
import { noteUnsupported, supported } from '../../lib/supports';
import type { PaneRow } from '../../lib/tree';
import { runKey } from '../../lib/tree';
import type { WorkspaceRow } from '../../lib/workspaces';
import { navigate, type WorkspaceRoute } from '../../router';
import { HandoffSheet } from '../handoff';
import { usePaneActions, type PaneActions } from '../pane/actions';
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

/** The tab the route selects (`show` = `term` / `conversation` / `preview:<id>`; else the pane's default). */
export function currentTabId(current: PaneRow, show: string | null | undefined, view: AgentView = 'conversation'): string {
  return paneTabId(current.pane.id, !!current.run, show, view);
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
  const view = agentViewFor(prefs, row.host, row.workspace.id);
  const overridden = hasViewOverride(prefs, row.host, row.workspace.id);
  const allTabs = useMemo(() => workspaceTabs(row, previews, view), [row, previews, view]);
  const tabs = locked ? allTabs.filter((x) => x.pane?.pane.id === current.pane.id && x.kind !== 'preview') : allTabs;
  const tabId = currentTabId(current, show, view);
  const tab: WsTab | null = allTabs.find((x) => x.id === tabId) ?? null;
  const body = tabBody(tabId, view);
  const hasAgents = locked ? !!current.run : row.panes.some((p) => p.run);
  /** What the toggle shows as on: the agent body on screen, else the workspace's view. */
  const shownView: AgentView = current.run && body !== 'preview' ? body : view;
  const [findOpen, setFindOpen] = useState(false);
  const [sheet, setSheet] = useState<null | 'new' | 'share' | 'handoff' | 'rename-pane' | 'close-pane' | 'rename-tab' | 'close-tab'>(null);

  const select = useCallback(
    (x: WsTab) => {
      const s =
        x.kind === 'preview'
          ? `preview:${x.preview!.id}`
          : x.kind === 'term' && x.pane?.run
            ? showFor('terminal', view)
            : x.kind === 'conv'
              ? showFor('conversation', view)
              : null;
      if (locked) return setLocalShow(s);
      navigate({ ...route, pane: x.pane?.pane.id ?? route.pane, show: s, view: null }, { replace: true });
    },
    [locked, route, view],
  );
  const openTerminal = () => {
    const s = showFor('terminal', view);
    if (locked) return setLocalShow(s);
    navigate({ ...route, pane: current.pane.id, show: s, view: null }, { replace: true });
  };

  /**
   * Switch the workspace's agent view on this device (null = back to the default) and show it:
   * the selected agent's primary tab, or the first agent's when a shell or preview is selected.
   */
  const setView = (v: AgentView | null) => {
    app.prefs.setWorkspaceView(row.host, row.workspace.id, v);
    if (locked) return setLocalShow(null);
    const target = current.run ? current : row.panes.find((p) => p.run);
    if (target) navigate({ ...route, pane: target.pane.id, show: null, view: null }, { replace: true });
  };
  const viewReq = useRef<(r: AgentViewRequest) => void>(() => {});
  viewReq.current = (r) => {
    if (!hasAgents) return;
    setView(r === 'toggle' ? otherView(shownView) : r === 'default' ? null : r);
  };
  useEffect(() => onAgentViewRequest((r) => viewReq.current(r)), []);

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
    hasAgents && overridden && { label: t.tabs2.useDefaultView(t.settings.agentViews[prefs.agentView]!), icon: <RotateCcw />, onSelect: () => setView(null) },
    body === 'terminal' && { label: t.tabs2.findInTerminal, icon: <Search />, onSelect: () => setFindOpen(true) },
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
        {body === 'terminal' && (
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
        viewToggle={hasAgents ? { value: shownView, mac, onChange: (v) => setView(v) } : null}
      />
      <div role="tabpanel" id={TAB_PANEL_ID} aria-labelledby={tab && tabs.some((x) => x.id === tab.id) ? tabDomId(tab.id) : undefined} className="flex min-h-0 flex-1 flex-col">
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
            mode={current.run && body === 'conversation' ? 'conversation' : 'terminal'}
            findOpen={findOpen}
            setFindOpen={setFindOpen}
            onOpenTerminal={openTerminal}
          />
        )}
      </div>

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
  /** `conversation` (agents only) or `terminal` (the pane's own screen, an agent's TUI too). */
  mode: AgentView;
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
  const term = mode === 'terminal';
  const [refreshKey, setRefreshKey] = useState(0);
  const burstRef = useRef<(() => void) | null>(null);
  const composerRef = useRef<HTMLDivElement>(null);
  const onSent = useCallback(() => {
    burstRef.current?.();
    setRefreshKey((k) => k + 1);
  }, []);
  const dialogsRef = useRef<HTMLDivElement>(null);
  /** Point at a dialog card (a failed send because one is open, or a slash command that opened one). */
  const focusDialog = useCallback((id: string | null) => {
    // The card may arrive with the next dashboard refresh: try now and once more shortly.
    const go = () => {
      const root = dialogsRef.current ?? document;
      const el = (id ? root.querySelector<HTMLElement>(`[data-interaction="${CSS.escape(id)}"]`) : null) ?? root.querySelector<HTMLElement>('[data-kind="picker"]');
      el?.scrollIntoView?.({ block: 'nearest' });
      el?.focus({ preventScroll: true });
      return !!el;
    };
    if (!go()) setTimeout(go, 400);
  }, []);
  const paneActions = usePaneActions(hostId, row.pane.id, run, scope, onSent, focusDialog);
  // The terminal types into the pane (`pane.send_text` + Enter), even when an agent runs there:
  // the user is talking to the agent's own interface, not prompting it through the host.
  const actions = useMemo<PaneActions>(() => (term ? { ...paneActions, text: (s, o) => paneActions.text(s, { ...o, raw: true }) } : paneActions), [paneActions, term]);
  const [text, setText] = useComposerDraft(hostId, row.pane.id, !term);
  const [belt, setBelt] = useState<BeltTab | null>(null);
  const [noEcho, setNoEcho] = useState(false);
  const cards = inbox.filter((it) => it.host_id === hostId && it.interaction.pane === row.pane.id);
  // In the conversation, an open picker / unknown dialog sits above the composer (which pauses);
  // in the terminal it stays in the dock with the other cards.
  const dialogs = !term ? cards.filter((c) => c.interaction.kind === 'picker') : [];
  const otherCards = !term ? cards.filter((c) => c.interaction.kind !== 'picker') : cards;

  // Viewing a finished run marks it seen.
  useEffect(() => {
    if (run) app.prefs.markSeen(runKey(hostId, run.id), run.done_rev);
  }, [run?.id, run?.done_rev]);

  /** Typing focus: the composer's text box (clicking the screen puts the caret there). */
  const focusComposer = useCallback(() => {
    composerRef.current?.querySelector<HTMLTextAreaElement>('textarea')?.focus({ preventScroll: true });
  }, []);
  // Switching to the terminal on a pointer device: ready to type.
  useEffect(() => {
    if (!term || !canType) return;
    if (typeof window !== 'undefined' && window.matchMedia?.('(pointer: coarse)').matches) return;
    const active = document.activeElement;
    // Keep focus where the user put it (a tab being walked with the arrow keys, a field…); only
    // from nowhere or from the view toggle does typing focus move to the composer.
    if (active && active !== document.body && !active.closest('[data-view-toggle]')) return;
    focusComposer();
  }, [term, canType]);

  const beltEl = (
    <ActionBelt
      tab={term ? belt : (belt ?? 'keys')}
      setTab={setBelt}
      actions={actions}
      harness={run?.harness ?? null}
      canType={canType}
      onInsert={(s) => setText((cur) => (cur ? `${cur} ${s}` : s))}
    />
  );

  return (
    <div className="flex min-h-0 flex-1 flex-col">
      {mode === 'conversation' && run ? (
        <Conversation
          hostId={hostId}
          pane={row.pane.id}
          run={run}
          cwd={run.cwd ?? row.pane.cwd}
          refreshKey={refreshKey}
          onOpenTerminal={onOpenTerminal}
          tail={otherCards.length > 0 ? <Approvals items={otherCards} onOpenTerminal={onOpenTerminal} /> : null}
        />
      ) : (
        <>
          <TerminalTab
            hostId={hostId}
            pane={row.pane.id}
            working={working}
            findOpen={findOpen}
            setFindOpen={setFindOpen}
            onNoEcho={setNoEcho}
            burstRef={burstRef}
            onActivate={canType ? focusComposer : undefined}
          />
          {cards.length > 0 && <ApprovalDock items={cards} onOpenTerminal={onOpenTerminal} />}
        </>
      )}
      <div className="shrink-0 pb-safe">
        {dialogs.length > 0 && (
          <div ref={dialogsRef} className="mx-auto max-h-[55vh] w-full max-w-[780px] overflow-y-auto px-3 pb-2 sm:px-4" data-dialogs>
            <Approvals items={dialogs} onOpenTerminal={onOpenTerminal} />
          </div>
        )}
        <div className="mx-auto w-full max-w-[780px] space-y-1 px-3 empty:hidden sm:px-4">
          {term && noEcho && canType && <Notice tone="warn">{t.composer.password}</Notice>}
          {!online && <Notice tone="warn">{t.composer.offline}</Notice>}
          {online && scope === 'view' && <Notice>{t.composer.readOnly}</Notice>}
          {online && scope === 'approve' && (
            <Notice
              action={
                composerShowsStop(run, host?.dashboard?.interactions, '') ? (
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
        {/* Terminal: the key belt stays on screen (keys, quick replies, agent commands). */}
        {term && <div data-belt>{beltEl}</div>}
        {canType && (
          <div ref={composerRef}>
            <Composer
              hostId={hostId}
              actions={actions}
              text={text}
              setText={setText}
              isAgent={!!run && !term}
              placeholder={term && run ? t.composer2.placeholderTerm : undefined}
              sttAvailable={host?.info?.features.includes('stt') ?? false}
              run={run}
              interactions={host?.dashboard?.interactions}
              more={term ? undefined : beltEl}
              locked={dialogs.length > 0}
            />
          </div>
        )}
      </div>
    </div>
  );
}

/** Approvals under the terminal: a collapsible dock, so they are never lost behind the screen. */
function ApprovalDock({ items, onOpenTerminal }: { items: InboxItem[]; onOpenTerminal?(): void }) {
  const [open, setOpen] = useState(true);
  return (
    <div className="shrink-0 border-t border-border bg-bg" data-approval-dock>
      <button type="button" aria-expanded={open} className="vk-focus flex h-9 w-full items-center gap-2 px-3 text-sm font-medium" onClick={() => setOpen(!open)}>
        <span className="size-2 rounded-full bg-need-strong" />
        {t.pane.cards(items.length)}
        <span className="flex-1" />
        {open ? <ChevronDown className="size-4 text-muted" /> : <ChevronUp className="size-4 text-muted" />}
      </button>
      {open && (
        <div className="max-h-[40vh] overflow-y-auto px-3 pb-2">
          <Approvals items={items} onOpenTerminal={onOpenTerminal} />
        </div>
      )}
    </div>
  );
}

/** Open approvals of the pane: one compact card each, or a batch card for identical requests. */
function Approvals({ items, onOpenTerminal }: { items: InboxItem[]; onOpenTerminal?(): void }) {
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
          <InteractionCard key={c.interaction.id} item={c} variant="compact" onOpenTerminal={onOpenTerminal} />
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

