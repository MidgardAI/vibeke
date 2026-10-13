// Messages waiting in the agent's own queue, above the composer: each shows as Queued, and Send
// now makes the harness take them right away (Claude Code and Codex). Styled like the held
// review notes (`held-drafts.tsx`).

import { useCallback, useEffect, useRef, useState } from 'react';
import { Clock, Send } from 'lucide-react';
import type { AgentRun, QueuedInput } from '@vibeke/core';
import { useApp } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { isQueueEvent, nothingQueued, queuedItems, queueUnsupported } from '../lib/queued';
import { Button } from './ui';

/** A slow poll backs up the events (a missed event, a reconnect). */
const POLL_MS = 10_000;

export function QueuedInputs({ hostId, run, canSend }: { hostId: string; run: AgentRun | null; canSend: boolean }) {
  const app = useApp();
  const [items, setItems] = useState<QueuedInput[]>([]);
  const [busy, setBusy] = useState(false);
  const unsupported = useRef(false);
  const runId = run && !run.ended_at_ms ? run.id : null;

  const refresh = useCallback(async () => {
    const conn = app.conn(hostId);
    if (!conn || !runId || unsupported.current) return;
    try {
      const r = await conn.request('agent.queued', { target: runId });
      if (r.run === runId || r.run === undefined) setItems(queuedItems(r));
    } catch (e) {
      if (queueUnsupported(e)) unsupported.current = true;
      // offline or a gone run: keep what is shown until the next answer
    }
  }, [app, hostId, runId]);

  useEffect(() => {
    setItems([]);
    if (!runId) return;
    void refresh();
    const timer = setInterval(() => void refresh(), POLL_MS);
    const off = app.manager.subscribeEvents(hostId, (e) => {
      if (isQueueEvent(e, runId)) void refresh();
    });
    return () => {
      clearInterval(timer);
      off();
    };
  }, [app, hostId, runId, refresh]);

  if (!runId || items.length === 0) return null;

  const sendNow = async () => {
    const conn = app.conn(hostId);
    if (!conn) return;
    setBusy(true);
    try {
      await conn.request('agent.send_now', { target: runId });
      app.toast(t.queued.sent, 'ok');
    } catch (e) {
      if (nothingQueued(e)) {
        app.toast(t.queued.nothing, 'info');
      } else {
        app.toast(`${t.queued.failed}: ${errorMessage(e)}`, 'error', 5000);
        app.haptic('error');
      }
    } finally {
      setBusy(false);
      void refresh();
    }
  };

  return (
    <section aria-label={t.queued.title(items.length)} className="mx-auto w-full max-w-[780px] px-3 pb-1 sm:px-4" data-queued>
      <div className="rounded-lg border border-border bg-surface p-2">
        <div className="flex items-center gap-2 text-xs font-medium text-muted">
          <Clock className="size-4" aria-hidden />
          <span className="flex-1">{t.queued.title(items.length)}</span>
          {canSend && (
            <Button size="sm" variant="outline" icon={<Send />} busy={busy} onClick={() => void sendNow()}>
              {t.queued.sendNow}
            </Button>
          )}
        </div>
        <ol className="mt-1 max-h-40 space-y-1 overflow-y-auto text-sm">
          {items.map((q) => (
            <li key={q.id} className="flex items-start gap-2" data-queued-item={q.id}>
              <span className="mt-0.5 shrink-0 rounded bg-bg px-1.5 text-[11px] font-medium text-muted">{t.queued.label}</span>
              <span className="min-w-0 flex-1 whitespace-pre-wrap break-words">{q.text || <span className="text-muted">{t.queued.noText}</span>}</span>
            </li>
          ))}
        </ol>
        <p className="mt-1 text-xs text-muted">{t.queued.hint}</p>
      </div>
    </section>
  );
}
