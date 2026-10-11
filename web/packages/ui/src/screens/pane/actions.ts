import { useMemo } from 'react';
import { OutcomeUnknownError, type AgentRun, type Scope } from '@vibeke/core';
import { useApp } from '../../app/hooks';
import { t } from '../../i18n';
import { errorMessage } from '../../lib/answer';
import { ResendMemory, isPaneMoving } from '../../lib/resend';
import { dialogOpen, promptInteraction } from '../../lib/pickers';

export interface PaneActions {
  /** Send keys (grammar tokens) in one call. */
  keys(keys: string[]): Promise<boolean>;
  /** Send text: `agent.prompt` when the pane runs an agent and scope is full, else typed + Enter. */
  text(text: string, opts?: { raw?: boolean }): Promise<boolean>;
  interrupt(): Promise<boolean>;
}

// One memory for the app: a resend after a lost answer may come from a remounted composer.
const resend = new ResendMemory();

export function usePaneActions(hostId: string, pane: string, run: AgentRun | null, scope: Scope, onSent: () => void, onDialog?: (interaction: string | null) => void): PaneActions {
  const app = useApp();
  return useMemo(() => {
    const wrap = async (f: (opId: string | undefined) => Promise<unknown>, isPrompt = false, sig = ''): Promise<boolean> => {
      const conn = app.conn(hostId);
      if (!conn) return false;
      try {
        const r = await f(sig ? resend.opIdFor(sig, Date.now()) : undefined);
        if (sig) resend.settle(sig);
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
        if (isPaneMoving(e)) {
          // Input waits while the agent moves; keep the text for after the move.
          app.toast(t.composer.paneMoving, 'info', 4000);
          return false;
        }
        // Never retried automatically (spec 16 §1.7): tell the user to check the screen. If they
        // send the same text again, it repeats this call's op_id, so the host runs it once.
        if (sig) {
          if (e instanceof OutcomeUnknownError) resend.remember(sig, e.opId, Date.now());
          else resend.settle(sig);
        }
        app.toast(e instanceof OutcomeUnknownError ? t.composer.sendUnknown : `${t.composer.sendFailed}: ${errorMessage(e)}`, 'error', 5000);
        app.haptic('error');
        onSent();
        return false;
      }
    };
    const op = (id: string | undefined) => (id ? { op_id: id } : {});
    return {
      keys: (keys) => wrap(() => app.conn(hostId)!.request('pane.send_keys', { pane, keys })),
      text: (text, opts = {}) => {
        const prompt = !!run && scope === 'full' && !opts.raw && !run.ended_at_ms;
        const sig = JSON.stringify([hostId, prompt ? `run:${run!.id}` : `pane:${pane}`, text]);
        return wrap(
          (id) =>
            prompt
              ? app.conn(hostId)!.request('agent.prompt', { target: run!.id, text, ...op(id) })
              : app.conn(hostId)!.request('pane.send_text', { pane, text, submit: true, ...op(id) }),
          prompt,
          sig,
        );
      },
      interrupt: () => wrap(() => app.conn(hostId)!.request('agent.interrupt', { target: run?.id ?? pane })),
    };
  }, [app, hostId, pane, run?.id, run?.ended_at_ms, scope, onSent, onDialog]);
}
