import { useMemo } from 'react';
import { OutcomeUnknownError, type AgentRun, type Scope } from '@vibeke/core';
import { useApp } from '../../app/hooks';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { dialogOpen, promptInteraction } from '../../lib/pickers';

export interface PaneActions {
  /** Send keys (grammar tokens) in one call. */
  keys(keys: string[]): Promise<boolean>;
  /** Send text: `agent.prompt` when the pane runs an agent and scope is full, else typed + Enter. */
  text(text: string, opts?: { raw?: boolean }): Promise<boolean>;
  interrupt(): Promise<boolean>;
}

export function usePaneActions(hostId: string, pane: string, run: AgentRun | null, scope: Scope, onSent: () => void, onDialog?: (interaction: string | null) => void): PaneActions {
  const app = useApp();
  return useMemo(() => {
    const wrap = async (f: () => Promise<unknown>, isPrompt = false): Promise<boolean> => {
      const conn = app.conn(hostId);
      if (!conn) return false;
      try {
        const r = await f();
        onSent();
        // A slash command may open a picker instead of starting a turn: that is a success; show the picker.
        const opened = isPrompt ? promptInteraction(r) : null;
        if (opened) onDialog?.(opened);
        return true;
      } catch (e) {
        // A dialog is open on the agent: nothing was typed. Keep the text, point at the dialog.
        const open = isPrompt ? dialogOpen(e) : null;
        if (open) {
          app.toast(t.picker.dialogOpen, 'info', 4000);
          onDialog?.(open.interaction);
          return false;
        }
        // Never retried (spec 16 §1.7): tell the user to check the screen.
        app.toast(e instanceof OutcomeUnknownError ? t.composer.sendUnknown : `${t.composer.sendFailed}: ${errorMessage(e)}`, 'error', 5000);
        app.haptic('error');
        onSent();
        return false;
      }
    };
    return {
      keys: (keys) => wrap(() => app.conn(hostId)!.request('pane.send_keys', { pane, keys })),
      text: (text, opts = {}) =>
        wrap(() =>
          run && scope === 'full' && !opts.raw && !run.ended_at_ms
            ? app.conn(hostId)!.request('agent.prompt', { target: run.id, text })
            : app.conn(hostId)!.request('pane.send_text', { pane, text, submit: true }),
          !!run && scope === 'full' && !opts.raw && !run.ended_at_ms,
        ),
      interrupt: () => wrap(() => app.conn(hostId)!.request('agent.interrupt', { target: run?.id ?? pane })),
    };
  }, [app, hostId, pane, run?.id, run?.ended_at_ms, scope, onSent, onDialog]);
}
