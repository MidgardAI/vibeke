// New agent / new tab sheet (spec 16 §9.1 Home): host, workspace, harness, optional first prompt.

import { useEffect, useState } from 'react';
import { displayName, type HarnessInfo } from '@vibeke/core';
import { useApp, useHosts } from '../app/hooks';
import { t } from '../i18n';
import { errorMessage } from '../lib/answer';
import { harnessLabel } from '../lib/harness';
import { takeNewAgentPrefill } from '../lib/new-agent-prefill';
import { navigate } from '../router';
import { Button, Notice, Segmented, Sheet, cx } from './ui';

export function NewSheet({ open, onClose, hostId, workspaceId }: { open: boolean; onClose(): void; hostId?: string; workspaceId?: string }) {
  const app = useApp();
  const hosts = useHosts().filter((h) => h.status === 'online' && (h.info?.scope ?? h.record.scope) === 'full');
  const [mode, setMode] = useState<'agent' | 'tab'>('agent');
  const [host, setHost] = useState<string | null>(hostId ?? null);
  const [ws, setWs] = useState<string | null>(workspaceId ?? null);
  const [harness, setHarness] = useState<string | null>(null);
  const [harnesses, setHarnesses] = useState<HarnessInfo[]>([]);
  const [prompt, setPrompt] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const h = hosts.find((x) => x.record.host_id === host) ?? hosts[0];
  const workspaces = h?.dashboard?.workspaces ?? [];
  const wsId = ws && workspaces.some((w) => w.id === ws) ? ws : (workspaces[0]?.id ?? null);

  // A prompt handed over by another screen (shared content).
  useEffect(() => {
    if (!open) return;
    const p = takeNewAgentPrefill();
    if (p) {
      setMode('agent');
      setPrompt(p.prompt);
    }
  }, [open]);

  useEffect(() => {
    if (!open || !h) return;
    let live = true;
    app
      .conn(h.record.host_id)
      ?.request('agent.harnesses', {})
      .then((r) => {
        if (!live) return;
        setHarnesses(r.harnesses);
        setHarness((cur) => cur ?? r.harnesses.find((x) => x.version_detected)?.id ?? r.harnesses[0]?.id ?? null);
      })
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [open, h?.record.host_id]);

  const submit = async () => {
    if (!h || !wsId) return;
    const conn = app.conn(h.record.host_id);
    if (!conn) return;
    setBusy(true);
    setError(null);
    try {
      let pane: string;
      if (mode === 'agent') {
        if (!harness) return;
        const r = await conn.request('agent.start', { workspace: wsId, harness, ...(prompt.trim() ? { prompt: prompt.trim() } : {}) });
        pane = r.pane;
      } else {
        const r = await conn.request('tab.create', { workspace: wsId });
        pane = r.root_pane.id;
      }
      app.haptic('success');
      onClose();
      setPrompt('');
      void conn.refresh().catch(() => {});
      navigate({ name: 'pane', host: h.record.host_id, pane, view: 'term' });
    } catch (e) {
      setError(errorMessage(e));
      app.haptic('error');
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet open={open} onClose={onClose} title={t.newAgent.title}>
      <div className="space-y-4">
        <Segmented<'agent' | 'tab'>
          label={t.newAgent.title}
          value={mode}
          onChange={setMode}
          options={[
            { value: 'agent', label: t.newAgent.agent },
            { value: 'tab', label: t.newAgent.tab },
          ]}
        />
        {hosts.length === 0 && <Notice tone="warn">{t.composer.offline}</Notice>}
        {hosts.length > 1 && (
          <Picker
            label={t.newAgent.host}
            value={h?.record.host_id ?? null}
            options={hosts.map((x) => ({ value: x.record.host_id, label: x.info?.host_name ?? x.record.name }))}
            onChange={setHost}
          />
        )}
        <Picker label={t.newAgent.workspace} value={wsId} options={workspaces.map((w) => ({ value: w.id, label: displayName(w) }))} onChange={setWs} />
        {mode === 'agent' && (
          <>
            <Picker
              label={t.newAgent.harness}
              value={harness}
              options={harnesses.map((x) => ({
                value: x.id,
                label: `${x.display || harnessLabel(x.id)}${x.version_detected ? ` ${x.version_detected}` : ` (${t.newAgent.notDetected})`}`,
              }))}
              onChange={setHarness}
            />
            <label className="block">
              <span className="mb-1 block text-sm text-muted">{t.newAgent.prompt}</span>
              <textarea
                value={prompt}
                onChange={(e) => setPrompt(e.target.value)}
                rows={3}
                className="w-full resize-none rounded-xl border border-border bg-bg px-3 py-2 text-base"
              />
            </label>
          </>
        )}
        {error && <Notice tone="danger">{error}</Notice>}
        <Button variant="primary" block size="lg" busy={busy} disabled={!h || !wsId || (mode === 'agent' && !harness)} onClick={() => void submit()}>
          {mode === 'agent' ? t.newAgent.start : t.newAgent.create}
        </Button>
      </div>
    </Sheet>
  );
}

function Picker({ label, value, options, onChange }: { label: string; value: string | null; options: { value: string; label: string }[]; onChange(v: string): void }) {
  return (
    <div>
      <div className="mb-1 text-sm text-muted">{label}</div>
      <div className="flex flex-wrap gap-1.5">
        {options.map((o) => (
          <button
            key={o.value}
            type="button"
            onClick={() => onChange(o.value)}
            aria-pressed={value === o.value}
            className={cx('h-9 rounded-full border px-3 text-sm', value === o.value ? 'border-accent bg-accent/10 text-fg' : 'border-border text-muted')}
          >
            {o.label}
          </button>
        ))}
      </div>
    </div>
  );
}
