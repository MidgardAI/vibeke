// One dev-server preview, in the Previews panel or (expanded) in the workspace centre (spec 06
// B3.5). The desktop app embeds the page (`EmbeddedPreview`); a phone shows the page rendered by
// the host's headless browser (`PhonePreview`); anything else shows the preview's address. Agent
// browser sessions can be watched from every layout.

import { useEffect, useState } from 'react';
import { ExternalLink, Eye, Globe, MonitorPlay } from 'lucide-react';
import type { BrowserSession, Preview } from '@vibeke/core';
import { useApp, useHost } from '../../app/hooks';
import { EmbeddedPreview } from '../../components/embedded-preview';
import { HeldDrafts } from '../../components/held-drafts';
import { PhonePreview } from '../../components/phone-preview';
import { Screencast } from '../../components/screencast';
import { Button, Empty, StatusDot, type Status } from '../../components/ui';
import { t } from '../../i18n';
import { isLoopbackUrl } from '../../lib/phone-preview';
import { previewKey, previewLabel } from '../../lib/previews';
import { sessionsFor } from '../../lib/screencast';

/** Status dots of previews (the panel's rows, the info card). */
export const PREVIEW_DOT: Record<string, Status> = { up: 'review', down: 'error', declared: 'idle', suggested: 'idle', gone: 'offline' };

export function PreviewView(props: Parameters<typeof PreviewBody>[0]) {
  if (!props.preview) return <PreviewBody {...props} />;
  // Held review notes from reviewers (spec 20 §3.3) sit above the page.
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <HeldDrafts hostId={props.hostId} previewId={props.preview.id} />
      <PreviewBody {...props} />
    </div>
  );
}

function PreviewBody({
  hostId,
  preview,
  expanded,
  onExpand,
}: {
  hostId: string;
  preview: Preview | null;
  /** In the centre (Collapse moves it back to the panel). */
  expanded?: boolean;
  /** Expand / Collapse; absent where the preview cannot move (the phone-sized sheet). */
  onExpand?: () => void;
}) {
  const app = useApp();
  const host = useHost(hostId);
  const [watching, setWatching] = useState<BrowserSession | null>(null);
  const [listing, setListing] = useState(false);
  if (!preview) return <Empty icon={<Globe />} title={t.tabs2.previewGone} />;
  const full = (host?.info?.scope ?? host?.record.scope) === 'full' && host?.status === 'online';
  if (watching) return <Screencast hostId={hostId} session={watching} canControl={full} onClose={() => setWatching(null)} />;
  if ('previewView' in app.platform && app.platform.previewView) return <EmbeddedPreview key={previewKey(hostId, preview)} hostId={hostId} preview={preview} expanded={expanded} onExpand={onExpand} />;
  const info = host?.info;
  const rendered = full && !!info && !info.limit?.pane && !info.limit?.workspace && info.kind !== 'share' && info.features.includes('browser_preview');
  const browserButton = !isLoopbackUrl(preview.url) && (
    <Button size="sm" variant="outline" icon={<ExternalLink />} onClick={() => app.platform.openExternal(preview.url)}>
      {t.tabs2.previewBrowser}
    </Button>
  );

  if (rendered) {
    return (
      <div className="flex min-h-0 flex-1 flex-col">
        <PhonePreview key={previewKey(hostId, preview)} hostId={hostId} preview={preview} expanded={expanded} onExpand={onExpand} />
        <div className="flex shrink-0 flex-wrap items-center gap-2 border-t border-border px-3 py-1.5">
          <span className="inline-flex min-w-0 flex-1 items-center gap-1.5 text-xs text-muted">
            <StatusDot status={PREVIEW_DOT[preview.status] ?? 'idle'} />
            <span className="truncate">{previewLabel(preview)}</span>
          </span>
          {browserButton}
          <Button size="sm" variant="ghost" icon={<MonitorPlay />} aria-expanded={listing} onClick={() => setListing((v) => !v)}>
            {listing ? t.phone.hideSessions : t.phone.agentSessions}
          </Button>
        </div>
        {listing && (
          <div className="vk-scroll max-h-[40%] shrink-0 overflow-y-auto px-3 pb-3">
            <LiveSessions hostId={hostId} preview={preview} onWatch={setWatching} showEmpty />
          </div>
        )}
      </div>
    );
  }

  return (
    <div className="vk-scroll flex min-h-0 flex-1 flex-col items-center justify-center gap-4 overflow-y-auto px-6 py-4">
      <div className="w-full max-w-md rounded-xl border border-border bg-surface p-4">
        <div className="flex items-center gap-2 text-sm">
          <Globe className="size-4 text-muted" />
          <span className="min-w-0 flex-1 truncate font-medium">{previewLabel(preview)}</span>
          <span className="inline-flex items-center gap-1.5 text-xs text-muted">
            <StatusDot status={PREVIEW_DOT[preview.status] ?? 'idle'} />
            {t.tabs2.previewStatus[preview.status] ?? preview.status}
          </span>
        </div>
        <div className="mt-2 break-all font-mono text-xs text-muted">{preview.url}</div>
        {browserButton && <div className="mt-4 flex flex-wrap gap-2">{browserButton}</div>}
      </div>
      <LiveSessions hostId={hostId} preview={preview} onWatch={setWatching} />
    </div>
  );
}

/** The agent browser sessions on the host that can be watched live (feature `browser_preview`). */
function LiveSessions({ hostId, preview, onWatch, showEmpty }: { hostId: string; preview: Preview; onWatch(s: BrowserSession): void; showEmpty?: boolean }) {
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
  if (!eligible) return null;
  if (!sessions?.length) return showEmpty && sessions ? <div className="text-xs text-muted">{t.live.noSessions}</div> : null;
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
