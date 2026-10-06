// A dev-server preview as a workspace tab: its URL and status, opened on the host (a browser
// pane next to the agent, `preview.open`) or in this device's browser.

import { useState } from 'react';
import { ExternalLink, Globe, MonitorPlay } from 'lucide-react';
import type { Preview } from '@vibeke/core';
import { useApp, useHost } from '../../app/hooks';
import { Button, Empty, StatusDot, type Status } from '../../components/ui';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { noteUnsupported, supported } from '../../lib/supports';
import { previewLabel } from './tab-strip';

const DOT: Record<string, Status> = { up: 'review', down: 'error', declared: 'idle', suggested: 'idle', gone: 'offline' };

export function PreviewTab({ hostId, preview }: { hostId: string; preview: Preview | null }) {
  const app = useApp();
  const host = useHost(hostId);
  const [busy, setBusy] = useState(false);
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
  return (
    <div className="flex min-h-0 flex-1 flex-col items-center justify-center px-6">
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
    </div>
  );
}
