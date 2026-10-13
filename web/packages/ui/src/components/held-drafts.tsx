// Held review notes on owner devices (spec 20 §3.3): a reviewer with `notes: held` sends a draft
// that waits here. The owner reads it and sends it to the agent or discards it. Shown above the
// preview in the Previews panel (desktop and phone alike).

import { useCallback, useEffect, useState } from 'react';
import { Inbox, Send, Trash2 } from 'lucide-react';
import { hostKind, type AnnotationDraft } from '@vibeke/core';
import { useApp, useHost } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { noteTitle } from '../lib/annotations';
import { Button } from './ui';

const POLL_MS = 5000;

/** Held drafts of a target's draft list (other states are the reviewers' business). */
export const heldDrafts = (drafts: readonly AnnotationDraft[] | undefined): AnnotationDraft[] =>
  (drafts ?? []).filter((d) => d && d.state === 'held').sort((a, b) => b.updated_at_ms - a.updated_at_ms);

export function HeldDrafts({ hostId, previewId }: { hostId: string; previewId: string }) {
  const app = useApp();
  const host = useHost(hostId);
  const [drafts, setDrafts] = useState<AnnotationDraft[]>([]);
  const [busy, setBusy] = useState<string | null>(null);
  const owner =
    !!host && host.status === 'online' && hostKind(host.record) === 'device' && (host.info?.scope ?? host.record.scope) === 'full' && !host.info?.limit?.pane && !host.info?.limit?.workspace;

  const refresh = useCallback(async () => {
    const conn = app.conn(hostId);
    if (!conn) return;
    try {
      const r = await conn.request('browser.annotations', { preview: previewId });
      setDrafts(heldDrafts(r.drafts));
    } catch {
      // older host or offline: keep the list
    }
  }, [app, hostId, previewId]);

  useEffect(() => {
    if (!owner) return;
    void refresh();
    const timer = setInterval(() => void refresh(), POLL_MS);
    const off = app.manager.subscribeEvents(hostId, (e) => {
      if (e.type.startsWith('browser.annotations')) void refresh();
    });
    return () => {
      clearInterval(timer);
      off();
    };
  }, [app, hostId, owner, refresh]);

  if (!owner || drafts.length === 0) return null;

  const act = async (d: AnnotationDraft, how: 'release' | 'discard') => {
    const conn = app.conn(hostId);
    if (!conn) return;
    setBusy(d.id);
    try {
      if (how === 'release') await conn.request('browser.annotations_release', { draft: d.id });
      else await conn.request('browser.annotations_discard', { draft: d.id });
      app.toast(how === 'release' ? t.held.released : t.held.discarded, 'ok');
      setDrafts((l) => l.filter((x) => x.id !== d.id));
      void refresh();
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    } finally {
      setBusy(null);
    }
  };

  return (
    <section aria-label={t.held.title} className="shrink-0 space-y-2 border-b border-border bg-surface px-3 py-2">
      <div className="flex items-center gap-2 text-xs font-medium text-muted">
        <Inbox className="size-4" aria-hidden />
        {t.held.title}
      </div>
      {drafts.map((d) => {
        // Everything that would be sent, in full: the owner approves exactly this text.
        const notes = d.notes;
        return (
          <div key={d.id} className="rounded-lg border border-border bg-bg p-2">
            <div className="text-sm font-medium">{t.held.from(d.author?.label ?? '?', notes.length + (d.page_note ? 1 : 0))}</div>
            <ol className="mt-1 max-h-64 list-decimal space-y-0.5 overflow-y-auto pl-5 text-xs text-muted">
              {notes.map((n) => (
                <li key={n.n} className="whitespace-pre-wrap break-words">
                  <span className="text-fg">{noteTitle(n)}</span>
                  {n.note ? `: ${n.note}` : ''}
                </li>
              ))}
            </ol>
            {d.page_note && <p className="mt-1 whitespace-pre-wrap break-words text-xs">{d.page_note}</p>}
            <div className="mt-2 flex flex-wrap gap-2">
              <Button size="sm" variant="primary" icon={<Send />} busy={busy === d.id} disabled={!!busy} onClick={() => void act(d, 'release')}>
                {t.held.release}
              </Button>
              <Button size="sm" variant="ghost" icon={<Trash2 />} disabled={!!busy} onClick={() => void act(d, 'discard')}>
                {t.held.discard}
              </Button>
            </div>
          </div>
        );
      })}
    </section>
  );
}
