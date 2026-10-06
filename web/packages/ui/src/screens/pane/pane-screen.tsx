// Pane view (spec 16 §9.1 Pane): mirror with find/copy/wrap/size, card dock of this pane's open
// interactions, action belt, composer; History and Changes as sub-views; zen mode.

import { useCallback, useEffect, useMemo, useState, type ReactNode } from 'react';
import {
  ArrowLeft,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  ChevronUp,
  Copy,
  FileDiff,
  History,
  Maximize2,
  Minimize2,
  MoreVertical,
  OctagonX,
  Search,
  Send,
  Share2,
  SquareTerminal,
  X,
} from 'lucide-react';
import { hostKind, paneTitle } from '@vibeke/core';
import { useApp, useHost, useInboxItems, usePrefs, useTree, useNow } from '../../app/hooks';
import { InteractionCard } from '../../components/interaction-card';
import { PaneMenu, stateWord } from '../../components/pane-row';
import { countMatches, TerminalMirror } from '../../components/terminal';
import { Button, Empty, IconButton, Notice, Segmented, Sheet, SheetRow, cx } from '../../components/ui';
import { t } from '../../i18n';
import { stripAnsi } from '../../lib/ansi';
import { ago } from '../../lib/format';
import { isNoEchoPrompt } from '../../lib/guards';
import { harnessLabel } from '../../lib/harness';
import { neighbours, runKey } from '../../lib/tree';
import { goBack, navigate, type PaneView } from '../../router';
import { ChangesPanel } from '../changes';
import { HandoffSheet } from '../handoff';
import { HistoryView } from '../history';
import { ShareSheet } from '../share';
import { usePaneActions } from './actions';
import { ActionBelt, type BeltTab } from './belt';
import { Composer } from './composer';
import { useMirror } from './use-mirror';

export function PaneScreen({ host, pane, view }: { host: string; pane: string; view: PaneView }) {
  return <PaneInner key={`${host}/${pane}`} hostId={host} paneId={pane} view={view} />;
}

function useLandscape(): boolean {
  const q = typeof window !== 'undefined' && window.matchMedia ? window.matchMedia('(orientation: landscape) and (max-height: 500px)') : null;
  const [v, setV] = useState(q?.matches ?? false);
  useEffect(() => {
    if (!q) return;
    const f = () => setV(q.matches);
    q.addEventListener('change', f);
    return () => q.removeEventListener('change', f);
  }, []);
  return v;
}

function PaneInner({ hostId, paneId, view }: { hostId: string; paneId: string; view: PaneView }) {
  const app = useApp();
  const prefs = usePrefs();
  const host = useHost(hostId);
  const tree = useTree();
  const inbox = useInboxItems();
  const now = useNow(5000);
  const row = tree.all.find((r) => r.host === hostId && r.pane.id === paneId);
  const nb = neighbours(tree, hostId, paneId);
  const run = row?.run ?? null;
  const scope = host?.info?.scope ?? host?.record.scope ?? 'view';
  const online = host?.status === 'online';
  const canType = scope === 'full' && online;
  const working = run?.execution.value === 'working' || run?.execution.value === 'starting';
  const mirror = useMirror(hostId, paneId, working);
  const onSent = useCallback(() => mirror.burst(), [mirror.burst]);
  const actions = usePaneActions(hostId, paneId, run, scope, onSent);
  const [text, setText] = useState('');
  const [belt, setBelt] = useState<BeltTab | null>(null);
  const [findOpen, setFindOpen] = useState(false);
  const [query, setQuery] = useState('');
  const [hit, setHit] = useState(0);
  const [menu, setMenu] = useState(false);
  const [paneMenu, setPaneMenu] = useState(false);
  const [shareOpen, setShareOpen] = useState(false);
  const [handoffOpen, setHandoffOpen] = useState(false);
  const [dockOpen, setDockOpen] = useState(true);
  const [zenManual, setZen] = useState(false);
  const landscape = useLandscape();
  const zen = zenManual || (prefs.zenLandscape && landscape);
  const cards = inbox.filter((it) => it.host_id === hostId && it.interaction.pane === paneId);
  const plain = useMemo(() => stripAnsi(mirror.text), [mirror.text]);
  const matches = useMemo(() => countMatches(plain, query), [plain, query]);
  const noEcho = useMemo(() => isNoEchoPrompt(plain), [plain]);

  // Viewing a finished run marks it seen.
  useEffect(() => {
    if (run) app.prefs.markSeen(runKey(hostId, run.id), run.done_rev);
  }, [run?.id, run?.done_rev]);

  if (host && host.dashboard && !row) {
    return (
      <div className="flex h-full flex-col pt-safe">
        <Header onBack={() => goBack({ name: 'panes' })} title={t.pane.notFound} sub="" />
        <Empty title={t.pane.notFound} action={<Button onClick={() => navigate({ name: 'panes' })}>{t.tabs.panes}</Button>} />
      </div>
    );
  }

  const title = row ? (row.pane.title ?? run?.name ?? paneTitle(row.pane)) : paneId;
  const sub = [run ? harnessLabel(run.harness) : null, row ? stateWord(row) : null, tree.hosts.length > 1 ? (host?.info?.host_name ?? host?.record.name) : null]
    .filter(Boolean)
    .join(' · ');

  const setView = (v: PaneView) => navigate({ name: 'pane', host: hostId, pane: paneId, view: v }, { replace: true });

  return (
    <div className="flex h-full flex-col bg-bg pt-safe">
      {!zen && (
        <>
          <Header
            onBack={() => goBack({ name: 'panes' })}
            title={title}
            sub={sub}
            right={
              <>
                <IconButton label={t.pane.prev} disabled={!nb.prev} onClick={() => nb.prev && navigate({ name: 'pane', host: hostId, pane: nb.prev.pane.id, view }, { replace: true })}>
                  <ChevronLeft className="size-5" />
                </IconButton>
                <IconButton label={t.pane.next} disabled={!nb.next} onClick={() => nb.next && navigate({ name: 'pane', host: hostId, pane: nb.next.pane.id, view }, { replace: true })}>
                  <ChevronRight className="size-5" />
                </IconButton>
                <IconButton label={t.more} onClick={() => setMenu(true)}>
                  <MoreVertical className="size-5" />
                </IconButton>
              </>
            }
          />
          <div className="flex justify-center border-b border-border pb-2">
            <Segmented
              label={t.pane.terminal}
              value={view}
              onChange={setView}
              options={[
                { value: 'term', label: t.pane.terminal },
                { value: 'history', label: t.pane.history },
                { value: 'changes', label: t.pane.changes },
              ]}
            />
          </div>
        </>
      )}

      {view === 'history' && <HistoryView hostId={hostId} run={run} />}
      {view === 'changes' && <ChangesPanel only={{ host: hostId, pane: paneId }} />}

      {view === 'term' && (
        <>
          {findOpen && (
            <div className="flex items-center gap-1 border-b border-border bg-surface px-2 py-1">
              <Search className="size-4 text-muted" />
              <input
                autoFocus
                value={query}
                onChange={(e) => {
                  setQuery(e.target.value);
                  setHit(0);
                }}
                placeholder={t.pane.findPlaceholder}
                className="h-9 min-w-0 flex-1 bg-transparent text-[15px] outline-none"
              />
              <span className="text-[12px] tabular-nums text-muted">{query ? `${matches ? hit + 1 : 0}/${matches}` : ''}</span>
              <IconButton label="previous" disabled={!matches} onClick={() => setHit((h) => (h - 1 + matches) % matches)}>
                <ChevronUp className="size-4" />
              </IconButton>
              <IconButton label="next" disabled={!matches} onClick={() => setHit((h) => (h + 1) % matches)}>
                <ChevronDown className="size-4" />
              </IconButton>
              <IconButton
                label={t.close}
                onClick={() => {
                  setFindOpen(false);
                  setQuery('');
                }}
              >
                <X className="size-4" />
              </IconButton>
            </div>
          )}
          {!online && mirror.at && <Notice tone="warn" className="m-2">{t.pane.offlineMirror(ago(mirror.at, now))}</Notice>}
          <div className="relative min-h-0 flex-1">
            {mirror.text ? (
              <TerminalMirror text={mirror.text} lines={mirror.lines} wrap={prefs.wrap} fontSize={prefs.termFont} find={query ? { query, current: hit } : undefined} className="absolute inset-0" />
            ) : (
              <div className="term absolute inset-0 flex items-center justify-center text-sm text-faint">{online ? t.loading : t.pane.noMirror}</div>
            )}
            {zen && (
              <button
                type="button"
                aria-label={t.pane.zen}
                onClick={() => setZen(false)}
                className="absolute right-2 top-2 inline-flex size-9 items-center justify-center rounded-full bg-surface/80 text-muted shadow"
              >
                <Minimize2 className="size-4" />
              </button>
            )}
          </div>

          {cards.length > 0 && (
            <div className="border-t border-border bg-bg">
              <button type="button" className="flex h-9 w-full items-center gap-2 px-3 text-[13px] font-medium" onClick={() => setDockOpen(!dockOpen)}>
                <span className="size-2 rounded-full bg-need-strong" />
                {t.pane.cards(cards.length)}
                <span className="flex-1" />
                {dockOpen ? <ChevronDown className="size-4" /> : <ChevronUp className="size-4" />}
              </button>
              {dockOpen && (
                <div className="max-h-[45vh] space-y-2 overflow-y-auto px-2 pb-2">
                  {cards.map((c) => (
                    <InteractionCard key={c.interaction.id} item={c} />
                  ))}
                </div>
              )}
            </div>
          )}

          {!zen && (
            <>
              {noEcho && canType && <Notice tone="warn" className="mx-2 mb-1">{t.composer.password}</Notice>}
              {!online && <Notice tone="warn" className="mx-2 mb-1">{t.composer.offline}</Notice>}
              {online && scope === 'view' && <Notice className="mx-2 mb-1">{t.composer.readOnly}</Notice>}
              {online && scope === 'approve' && (
                <Notice
                  className="mx-2 mb-1"
                  action={
                    run && working ? (
                      <Button size="sm" variant="outline" icon={<OctagonX className="size-3.5" />} onClick={() => void actions.interrupt()}>
                        {t.pane.interrupt}
                      </Button>
                    ) : undefined
                  }
                >
                  {t.composer.approveOnly}
                </Notice>
              )}
              <div className="pb-safe">
                <ActionBelt
                  tab={belt}
                  setTab={setBelt}
                  actions={actions}
                  harness={run?.harness ?? null}
                  canType={canType}
                  onInsert={(s) => setText((cur) => (cur ? `${cur} ${s}` : s))}
                  zen={zen}
                  setZen={setZen}
                />
                {canType && (
                  <Composer
                    hostId={hostId}
                    actions={actions}
                    text={text}
                    setText={setText}
                    isAgent={!!run}
                    sttAvailable={host?.info?.features.includes('stt') ?? false}
                  />
                )}
              </div>
            </>
          )}
        </>
      )}

      <Sheet open={menu} onClose={() => setMenu(false)} title={title}>
        <SheetRow icon={<Search className="size-5" />} onClick={() => (setMenu(false), setView('term'), setFindOpen(true))}>
          {t.pane.find}
        </SheetRow>
        <SheetRow icon={<SquareTerminal className="size-5" />} onClick={() => (setMenu(false), setView('term'))}>
          {t.pane.terminal}
        </SheetRow>
        <SheetRow icon={<History className="size-5" />} onClick={() => (setMenu(false), setView('history'))}>
          {t.pane.history}
        </SheetRow>
        <SheetRow icon={<FileDiff className="size-5" />} onClick={() => (setMenu(false), setView('changes'))}>
          {t.pane.changes}
        </SheetRow>
        <SheetRow
          icon={<Copy className="size-5" />}
          onClick={() => {
            setMenu(false);
            void app.platform.clipboard.writeText(plain).then(() => app.toast(t.copied, 'ok'));
          }}
        >
          {t.pane.copyScreen}
        </SheetRow>
        <SheetRow icon={<Maximize2 className="size-5" />} onClick={() => (setMenu(false), setView('term'), setZen(true))}>
          {t.pane.zen}
        </SheetRow>
        {run && scope !== 'view' && (
          <SheetRow icon={<OctagonX className="size-5" />} disabled={!online} onClick={() => (setMenu(false), void actions.interrupt())}>
            {t.pane.interrupt}
          </SheetRow>
        )}
        {row && canType && hostKind(host!.record) === 'device' && (
          <>
            <SheetRow icon={<Share2 className="size-5" />} onClick={() => (setMenu(false), setShareOpen(true))}>
              {t.share.action}
            </SheetRow>
            <SheetRow icon={<Send className="size-5" />} onClick={() => (setMenu(false), setHandoffOpen(true))}>
              {t.handoff.action}
            </SheetRow>
          </>
        )}
        {row && (
          <SheetRow icon={<MoreVertical className="size-5" />} onClick={() => (setMenu(false), setPaneMenu(true))}>
            {t.panes.pin} · {t.panes.rename} · {t.panes.close}
          </SheetRow>
        )}
      </Sheet>
      {row && <PaneMenu row={row} open={paneMenu} onClose={() => setPaneMenu(false)} />}
      {row && <ShareSheet row={row} open={shareOpen} onClose={() => setShareOpen(false)} />}
      {row && <HandoffSheet row={row} open={handoffOpen} onClose={() => setHandoffOpen(false)} />}
    </div>
  );
}

function Header({ onBack, title, sub, right }: { onBack(): void; title: string; sub: string; right?: ReactNode }) {
  return (
    <div className="flex items-center gap-1 px-1 py-1">
      <IconButton label={t.back} onClick={onBack}>
        <ArrowLeft className="size-5" />
      </IconButton>
      <div className="min-w-0 flex-1">
        <div className="truncate text-[15px] font-semibold">{title}</div>
        {sub && <div className={cx('truncate text-[12px] text-muted')}>{sub}</div>}
      </div>
      {right}
    </div>
  );
}
