// Model switcher: the composer's `harness · model` label as a button. Hosts that can list models
// (`agent.models`) show them in a sheet and switch with `agent.set_model`; otherwise the button
// sends `/model` and the agent's own picker appears as a card.

import { useEffect, useState } from 'react';
import { Check } from 'lucide-react';
import type { AgentModel, AgentRun } from '@vibeke/core';
import { useApp } from '../../app/hooks';
import { HarnessIcon, Notice, Sheet, Spinner, cx } from '../../components/ui';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { harnessLabel } from '../../lib/harness';
import { loadModels, switchModel } from '../../lib/pickers';
import type { PaneActions } from './actions';

/** Harnesses (per host) that cannot list models: go straight to `/model` next time. */
const noStructured = new Set<string>();

export function ModelSwitcher({ hostId, run, actions, disabled }: { hostId: string; run: AgentRun; actions: PaneActions; disabled?: boolean }) {
  const app = useApp();
  const [open, setOpen] = useState(false);
  const [state, setState] = useState<{ phase: 'loading' } | { phase: 'ready'; models: AgentModel[] } | { phase: 'error'; message: string }>({ phase: 'loading' });
  const [busy, setBusy] = useState<string | null>(null);
  const memo = `${hostId}/${run.harness}`;

  const slash = async () => {
    noStructured.add(memo);
    setOpen(false);
    await actions.text('/model');
  };

  const load = async () => {
    const conn = app.conn(hostId);
    if (!conn) return setState({ phase: 'error', message: t.modelSwitch.failed });
    setState({ phase: 'loading' });
    const r = await loadModels(conn, run.id);
    if (r.kind === 'models') setState({ phase: 'ready', models: r.models });
    else if (r.kind === 'fallback') await slash();
    else setState({ phase: 'error', message: r.message });
  };

  const tap = () => {
    if (disabled) return;
    app.haptic('tap');
    if (noStructured.has(memo)) {
      void actions.text('/model');
      return;
    }
    setOpen(true);
  };

  useEffect(() => {
    if (open) void load();
  }, [open]);

  const choose = async (m: AgentModel) => {
    const conn = app.conn(hostId);
    if (!conn || busy) return;
    if (m.current) return setOpen(false);
    setBusy(m.id);
    try {
      const r = await switchModel(conn, actions.text, run.id, m.id);
      if (r === 'set') {
        app.toast(t.modelSwitch.switched(m.label), 'ok');
        void conn.refresh().catch(() => {});
      } else if (r === 'picker') noStructured.add(memo);
      setOpen(false);
    } catch (e) {
      app.toast(errorMessage(e), 'error');
    } finally {
      setBusy(null);
    }
  };

  return (
    <>
      <button
        type="button"
        disabled={disabled}
        aria-label={t.modelSwitch.button}
        aria-haspopup="dialog"
        title={t.modelSwitch.button}
        onClick={tap}
        className="vk-focus inline-flex h-7 min-w-0 items-center gap-1.5 rounded-md px-1.5 text-xs text-muted hover:bg-hover hover:text-fg disabled:opacity-50"
      >
        <HarnessIcon harness={run.harness} />
        <span className="truncate">
          {harnessLabel(run.harness)}
          {run.model && <span className="text-faint"> · {run.model}</span>}
        </span>
      </button>
      <Sheet open={open} onClose={() => setOpen(false)} title={t.modelSwitch.title}>
        {state.phase === 'loading' && (
          <div className="flex items-center gap-2 py-3 text-sm text-muted">
            <Spinner /> {t.modelSwitch.loading}
          </div>
        )}
        {state.phase === 'error' && <Notice tone="warn">{state.message}</Notice>}
        {state.phase === 'ready' && (
          <div role="radiogroup" aria-label={t.modelSwitch.title} className="flex flex-col gap-1.5">
            {state.models.map((m) => (
              <button
                key={m.id}
                type="button"
                role="radio"
                aria-checked={m.current}
                disabled={!!busy}
                onClick={() => void choose(m)}
                className={cx('vk-focus flex items-center gap-2 rounded-xl border px-3 py-2 text-left text-sm disabled:opacity-60', m.current ? 'border-accent bg-accent/10' : 'border-border bg-bg')}
              >
                <span className="min-w-0 flex-1">
                  <span className="block font-medium">{m.label}</span>
                  {m.description && <span className="block text-xs text-muted">{m.description}</span>}
                </span>
                {busy === m.id ? <Spinner /> : m.current && <Check className="size-4 text-accent" aria-label={t.modelSwitch.current} />}
              </button>
            ))}
          </div>
        )}
      </Sheet>
    </>
  );
}
