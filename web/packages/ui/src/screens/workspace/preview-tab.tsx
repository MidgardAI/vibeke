// A dev-server preview as a workspace tab: its URL and status, opened on the host (a browser
// pane next to the agent, `preview.open`) or in this device's browser.

import { useEffect, useState } from 'react';
import { ExternalLink, Eye, Globe, MonitorPlay } from 'lucide-react';
import type { BrowserSession, Preview } from '@vibeke/core';
import { useApp, useHost } from '../../app/hooks';
import { Screencast } from '../../components/screencast';
import { Button, Empty, StatusDot, type Status } from '../../components/ui';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { sessionsFor } from '../../lib/screencast';
import { noteUnsupported, supported } from '../../lib/supports';
import { previewLabel } from './tab-strip';

const DOT: Record<string, Status> = { up: 'review', down: 'error', declared: 'idle', suggested: 'idle', gone: 'offline' };

export function PreviewTab({ hostId, preview }: { hostId: string; preview: Preview | null }) {
  const app = useApp();
  const host = useHost(hostId);
  const [busy, setBusy] = useState(false);
  const [watching, setWatching] = useState<BrowserSession | null>(null);
  if (!preview) return <Empty icon={<Globe />} title={t.tabs2.previewGone} />;
  const full = (host?.info?.scope ?? host?.record.scope) === 'full' && host?.status === 'online';
  const canHost = full && supported(hostId, 'preview.open');
  const openOnHost = async () => {
    const conn = app.conn(hostId);
    if (!conn) return;
    setBusy(true);
    try {
      await conn.request('preview.open', { preview: preview.id });
      app.toast(t.tabs2.previewOpen, 'ok');
    } catch (e) {
      if (!noteUnsupported(hostId, 'preview.open', e)) app.toast(errorMessage(e), 'error');
    } finally {
      setBusy(false);
    }
  };
  if (watching) return <Screencast hostId={hostId} session={watching} canControl={full} onClose={() => setWatching(null)} />;
  return (
    <div className="vk-scroll flex min-h-0 flex-1 flex-col items-center justify-center gap-4 overflow-y-auto px-6 py-4">
      <div className="w-full max-w-md rounded-xl border border-border bg-surface p-4">
        <div className="flex items-center gap-2 text-sm">
          <Globe className="size-4 text-muted" />
          <span className="min-w-0 flex-1 truncate font-medium">{previewLabel(preview)}</span>
          <span className="inline-flex items-center gap-1.5 text-xs text-muted">
            <StatusDot status={DOT[preview.status] ?? 'idle'} />
            {t.tabs2.previewStatus[preview.status] ?? preview.status}
          </span>
        </div>
        <div className="mt-2 break-all font-mono text-xs text-muted">{preview.url}</div>
        <div className="mt-4 flex flex-wrap gap-2">
          {canHost && (
            <Button size="sm" variant="primary" icon={<MonitorPlay />} busy={busy} onClick={() => void openOnHost()}>
              {t.tabs2.previewOpen}
            </Button>
          )}
          <Button size="sm" variant="outline" icon={<ExternalLink />} onClick={() => app.platform.openExternal(preview.url)}>
            {t.tabs2.previewBrowser}
          </Button>
        </div>
      </div>
      <LiveSessions hostId={hostId} preview={preview} onWatch={setWatching} />
    </div>
  );
}

/** The agent browser sessions on the host that can be watched live (feature `browser_preview`). */
function LiveSessions({ hostId, preview, onWatch }: { hostId: string; preview: Preview; onWatch(s: BrowserSession): void }) {
  const app = useApp();
  const host = useHost(hostId);
  const [sessions, setSessions] = useState<BrowserSession[] | null>(null);
  const info = host?.info;
  const eligible = host?.status === 'online' && !!info && info.features.includes('browser_preview') && !info.limit?.pane && !info.limit?.workspace && info.kind !== 'share';
  useEffect(() => {
    if (!eligible) return;
    let live = true;
    app
      .conn(hostId)
      ?.request('browser.list', {})
      .then((r) => live && setSessions(r.sessions))
      .catch(() => live && setSessions([]));
    return () => {
      live = false;
    };
  }, [app, hostId, eligible]);
  if (!eligible || !sessions?.length) return null;
  const list = sessionsFor(sessions, preview.url);
  return (
    <div className="w-full max-w-md rounded-xl border border-border bg-surface p-4">
      <div className="text-xs font-medium text-muted">{t.live.sessions}</div>
      <div className="mt-2 space-y-2">
        {list.map((s) => (
          <div key={s.session} className="flex items-center gap-2">
            <span className="min-w-0 flex-1 truncate font-mono text-xs" title={s.url}>
              {s.url}
            </span>
            <Button size="sm" variant="primary" icon={<Eye />} onClick={() => onWatch(s)}>
              {t.live.watch}
            </Button>
          </div>
        ))}
      </div>
    </div>
  );
}
