// Content shared into the app from another app (Web Share Target, route `#/share-in/<id>`): shows
// what arrived and sends it to a running agent (text goes into its composer draft, files are
// uploaded as attachments) or starts a new agent with it. The shell stores the shared data under
// the one-time id and `platform.takeShared` hands it over once.

import { useEffect, useState } from 'react';
import { FileText, Plus } from 'lucide-react';
import { useApp, useHosts } from '../app/hooks';
import { useWorkspaceRows } from '../app/selection';
import { emitUi } from '../app/keyboard';
import { Button, Card, Empty, Notice, SectionLabel, Spinner } from '../components/ui';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { base64Std } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { setNewAgentPrefill } from '../lib/new-agent-prefill';
import { appendToDraft, sharedText } from '../lib/shared';
import type { PaneRow } from '../lib/tree';
import type { SharedItem } from '../platform';
import { navigate, workspaceRoute } from '../router';

const MAX_FILE = 8 * 1024 * 1024;

/** `takeShared` deletes the data, so keep the first result for a re-render or a strict-mode remount. */
const taken = new Map<string, Promise<SharedItem | null>>();

export function ShareInScreen({ id }: { id: string }) {
  const app = useApp();
  const take = app.platform.takeShared;
  const [item, setItem] = useState<SharedItem | null | undefined>(undefined);
  useEffect(() => {
    if (!take) return setItem(null);
    let live = true;
    let p = taken.get(id);
    if (!p) taken.set(id, (p = take(id).catch(() => null)));
    void p.then((v) => live && setItem(v));
    return () => {
      live = false;
    };
  }, [id]);

  if (!take) return <Empty title={t.shareIn.unsupported} />;
  if (item === undefined) return <div className="flex justify-center py-16"><Spinner /></div>;
  if (!item) return <Empty title={t.shareIn.missing} action={<Button onClick={() => navigate({ name: 'inbox' }, { replace: true })}>{t.done}</Button>} />;
  return <Shared item={item} />;
}

function Shared({ item }: { item: SharedItem }) {
  const app = useApp();
  const hosts = useHosts();
  const rows = useWorkspaceRows();
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const text = sharedText(item);
  const agents = rows.flatMap((r) => r.panes.filter((p) => p.run).map((p) => ({ row: r, pane: p })));
  const online = (host: string) => hosts.find((h) => h.record.host_id === host)?.status === 'online';

  const sendTo = async (p: PaneRow, workspace: string) => {
    setError(null);
    setBusy(p.key);
    try {
      const conn = app.conn(p.host);
      if (item.files.length && !conn) throw new Error(t.shareIn.offline);
      const paths: string[] = [];
      for (const f of item.files) {
        if (f.blob.size > MAX_FILE) throw new Error(`${t.shareIn.uploadFailed(f.name)}: > 8 MiB`);
        try {
          const data = new Uint8Array(await f.blob.arrayBuffer());
          const r = await conn!.request('attachment.put', { name: f.name, mime: f.type || 'application/octet-stream', data_b64: base64Std(data) }, { timeoutMs: 120_000 });
          paths.push(r.path);
        } catch (e) {
          throw new Error(`${t.shareIn.uploadFailed(f.name)}: ${errorMessage(e)}`);
        }
      }
      const add = [text, paths.join(' ')].filter(Boolean).join('\n');
      const drafts = app.platform.drafts;
      if (drafts) await drafts.set(p.host, p.pane.id, appendToDraft(await drafts.get(p.host, p.pane.id).catch(() => ''), add));
      app.haptic('success');
      app.toast(t.shareIn.sent, 'info', 4000);
      navigate(workspaceRoute(p.host, workspace, { pane: p.pane.id, show: 'conversation' }), { replace: true });
    } catch (e) {
      setError(errorMessage(e));
      app.haptic('error');
    } finally {
      setBusy(null);
    }
  };

  const startNew = () => {
    setNewAgentPrefill({ prompt: text });
    navigate({ name: 'inbox' }, { replace: true });
    emitUi('new-agent');
  };

  return (
    <div className="space-y-2 pb-6">
      <SectionLabel>{t.shareIn.preview}</SectionLabel>
      <div className="px-4 sm:px-6">
        <Card className="space-y-2 p-3 text-sm">
          {text && <p className="whitespace-pre-wrap break-words">{text}</p>}
          {item.files.length > 0 && (
            <ul className="space-y-1 text-muted">
              {item.files.map((f, i) => (
                <li key={i} className="flex items-center gap-2"><FileText className="size-4 shrink-0" /><span className="truncate">{f.name}</span></li>
              ))}
            </ul>
          )}
        </Card>
      </div>
      {error && <div className="px-4 sm:px-6"><Notice tone="danger">{error}</Notice></div>}
      <SectionLabel>{t.shareIn.sendTo}</SectionLabel>
      <div className="space-y-1 px-4 sm:px-6">
        {agents.length === 0 && <p className="text-sm text-muted">{t.shareIn.noAgents}</p>}
        {agents.map(({ row, pane }) => (
          <button
            key={pane.key}
            type="button"
            disabled={busy !== null || !online(pane.host)}
            onClick={() => void sendTo(pane, row.workspace.id)}
            className="vk-focus flex min-h-11 w-full items-center gap-2 rounded-lg border border-border px-3 py-2 text-left text-sm hover:bg-hover disabled:opacity-50"
          >
            <span className="min-w-0 flex-1">
              <span className="block truncate">{row.title}</span>
              <span className="block truncate text-xs text-muted">{harnessLabel(pane.run?.harness)} · {row.hostName}</span>
            </span>
            {busy === pane.key && <Spinner />}
          </button>
        ))}
      </div>
      <div className="space-y-2 px-4 pt-3 sm:px-6">
        <Button block variant="outline" icon={<Plus className="size-4" />} disabled={busy !== null || !text || item.files.length > 0} onClick={startNew}>{t.shareIn.newAgent}</Button>
        {item.files.length > 0 && <p className="text-xs text-muted">{t.shareIn.newAgentFiles}</p>}
        <Button block variant="ghost" onClick={() => navigate({ name: 'inbox' }, { replace: true })}>{t.shareIn.discard}</Button>
      </div>
    </div>
  );
}
