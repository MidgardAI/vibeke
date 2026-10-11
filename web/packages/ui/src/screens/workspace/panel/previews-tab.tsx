// The Previews tab (spec 06 B3.5): the workspace's dev-server previews. Live and declared ones
// first, then discovery's suggestions in grey with an Open button (`preview.promote`). Each row's
// menu forgets the preview (`preview.forget`); gone previews are not listed. Selecting a preview
// shows it here, with a way back to the list. Expand moves it into the workspace centre (the same
// desktop view, new bounds); the phone-sized sheet shows it full screen instead.

import { useMemo, useState } from 'react';
import { ChevronLeft, MonitorPlay, MoreHorizontal, Play, Trash2 } from 'lucide-react';
import type { Preview } from '@vibeke/core';
import { useApp, useHost } from '../../../app/hooks';
import { useWorkspaceRows } from '../../../app/selection';
import { Button, Empty, IconButton, StatusDot, cx } from '../../../components/ui';
import { t } from '../../../i18n';
import { errorMessage } from '../../../lib/answer';
import { expandPreviewRoute, isSuggested, machineParam, previewLabel, selectPreviewRoute, selectedPreview, workspacePreviews } from '../../../lib/previews';
import { navigate, type WorkspaceRoute } from '../../../router';
import { MenuButton } from '../menu';
import { PREVIEW_DOT, PreviewView } from '../preview-view';
import { paneLabel } from '../tab-strip';

export function PreviewsTab({ route, sheet = false }: { route: WorkspaceRoute; sheet?: boolean }) {
  const app = useApp();
  const host = useHost(route.host);
  const rows = useWorkspaceRows();
  const row = rows.find((r) => r.host === route.host && r.workspace.id === route.workspace);
  const previews = useMemo(() => (row ? workspacePreviews(row, host?.dashboard?.previews) : []), [row, host?.dashboard?.previews]);
  const full = (host?.info?.scope ?? host?.record.scope) === 'full' && host?.status === 'online';
  const [busy, setBusy] = useState<string | null>(null);
  // In the phone sheet a preview is never expanded: the sheet shows it itself.
  const go = (r: WorkspaceRoute) => navigate(sheet ? { ...r, expanded: false } : r, { replace: true });
  // A limited device may act only on its shared local previews, and the gateway refuses any
  // machine selector from it.
  const target = (v: Preview) => (host?.info?.limit ? {} : machineParam(v));
  const selected = selectedPreview(route, previews);

  if (selected && (sheet || !route.expanded)) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        <div className="flex h-9 shrink-0 items-center gap-1.5 border-b border-border pl-1 pr-2">
          <IconButton label={t.previews.back} onClick={() => go(selectPreviewRoute(route, null))}>
            <ChevronLeft />
          </IconButton>
          <StatusDot status={PREVIEW_DOT[selected.status] ?? 'idle'} label={t.tabs2.previewStatus[selected.status] ?? selected.status} />
          <span className="min-w-0 flex-1 truncate text-[13px] font-medium" title={selected.url}>
            {previewLabel(selected)}
          </span>
          <span className="shrink-0 font-mono text-xs text-muted">:{selected.port}</span>
        </div>
        <PreviewView key={selected.id} hostId={route.host} preview={selected} onExpand={sheet ? undefined : () => go(expandPreviewRoute(route))} />
      </div>
    );
  }

  if (!previews.length) return <Empty icon={<MonitorPlay />} title={t.previews.empty} hint={t.previews.emptyHint} />;

  const conn = app.conn(route.host);
  const promote = async (v: Preview) => {
    if (!conn) return;
    setBusy(v.id);
    try {
      await conn.request('preview.promote', { preview: v.id, ...target(v) });
      void conn.refresh().catch(() => {});
      go(selectPreviewRoute(route, v.id));
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    } finally {
      setBusy(null);
    }
  };
  const forget = async (v: Preview) => {
    if (!conn) return;
    try {
      await conn.request('preview.forget', { preview: v.id, ...target(v) });
      if (route.preview === v.id) go(selectPreviewRoute(route, null));
      void conn.refresh().catch(() => {});
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    }
  };

  return (
    <div className="vk-scroll min-h-0 flex-1 overflow-y-auto py-1">
      <ul aria-label={t.previews.list} className="flex flex-col">
        {previews.map((v) => {
          const suggested = isSuggested(v);
          const owner = row?.panes.find((p) => p.pane.id === v.pane);
          const current = route.expanded && route.preview === v.id;
          const label = previewLabel(v);
          const body = (
            <>
              <StatusDot status={PREVIEW_DOT[v.status] ?? 'idle'} label={t.tabs2.previewStatus[v.status] ?? v.status} />
              <span className="min-w-0 flex-1">
                <span className="flex min-w-0 items-baseline gap-1.5">
                  <span className="truncate text-[13px]">{label}</span>
                  <span className="shrink-0 font-mono text-xs text-muted">:{v.port}</span>
                </span>
                <span className="block truncate text-xs text-muted">
                  {[owner ? paneLabel(owner) : null, current ? t.previews.expanded : suggested ? t.tabs2.previewStatus.suggested : null].filter(Boolean).join(' · ')}
                </span>
              </span>
            </>
          );
          return (
            <li key={v.id} data-preview={v.id} className={cx('group flex min-w-0 items-center gap-1 px-1.5', suggested && 'text-muted')}>
              {suggested ? (
                <div className="flex min-w-0 flex-1 items-center gap-2.5 rounded-md px-2 py-1.5" title={v.url}>
                  {body}
                </div>
              ) : (
                <button
                  type="button"
                  title={v.url}
                  aria-current={current || undefined}
                  onClick={() => go(selectPreviewRoute(route, v.id))}
                  className={cx('vk-focus flex min-w-0 flex-1 items-center gap-2.5 rounded-md px-2 py-1.5 text-left hover:bg-hover', current && 'bg-selected')}
                >
                  {body}
                </button>
              )}
              {suggested && full && (
                <Button size="sm" variant="outline" icon={<Play />} busy={busy === v.id} onClick={() => void promote(v)}>
                  {t.previews.open}
                </Button>
              )}
              {full && (
                <MenuButton
                  label={t.previews.rowMenu(label)}
                  icon={<MoreHorizontal />}
                  items={[{ label: t.previews.forget, icon: <Trash2 />, tone: 'danger', onSelect: () => void forget(v) }]}
                />
              )}
            </li>
          );
        })}
      </ul>
    </div>
  );
}
