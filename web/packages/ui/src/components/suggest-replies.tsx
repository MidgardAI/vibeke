// "Suggest replies" for an agent pane: asks the host's assistant (through the preview and confirm
// step) for short replies and shows them as chips. A chip only fills the message box; the user
// sends it. The answer is kept per pane until the run's next turn. Hidden when this device may not
// use the assistant, the host has it off, or the run is not waiting for the user.

import { useEffect, useState } from 'react';
import { Sparkles } from 'lucide-react';
import type { AgentRun } from '@vibeke/core';
import { turnStamp, replyCache } from '../lib/assist-access';
import { parseReplies } from '../lib/assist-flow';
import { useAssistAvailable, useAssistFlow } from '../lib/use-assist';
import { t } from '../i18n';
import { AssistPanel } from './assist-flow';
import { Button, cx } from './ui';

/** The run can be answered now: an agent that has stopped working. */
export const canSuggestFor = (run: AgentRun | null | undefined): run is AgentRun => !!run && !run.ended_at_ms && run.execution.value === 'idle';

export function SuggestReplies({
  hostId,
  pane,
  run,
  onPick,
  className,
}: {
  hostId: string;
  pane: string;
  run: AgentRun | null | undefined;
  /** Put the reply in the message box. */
  onPick(text: string): void;
  className?: string;
}) {
  const available = useAssistAvailable(hostId);
  const { flow, state } = useAssistFlow(hostId);
  const key = `${hostId}/${pane}`;
  const stamp = run ? turnStamp(run) : '';
  const [replies, setReplies] = useState<string[] | null>(() => replyCache.get(key, stamp));

  // A new turn or another pane: the old suggestions no longer fit.
  useEffect(() => {
    setReplies(replyCache.get(key, stamp));
    flow.reset();
  }, [key, stamp, flow]);

  useEffect(() => {
    if (state.phase !== 'done') return;
    const list = parseReplies(state.output);
    replyCache.set(key, stamp, list);
    setReplies(list);
    flow.reset();
  }, [state.phase, state.output, key, stamp, flow]);

  if (!available || !canSuggestFor(run)) return null;
  const runId = run.id;
  const start = () => void flow.start({ operation: 'reply_suggestions', pane, run: runId });

  return (
    <div className={cx('space-y-1.5', className)} data-suggest-replies>
      {replies && replies.length > 0 && state.phase === 'idle' && (
        <div role="group" aria-label={t.replies.label}>
          <div className="flex flex-wrap gap-1.5">
            {replies.map((r) => (
              <button
                key={r}
                type="button"
                onClick={() => onPick(r)}
                className="vk-focus inline-flex min-h-9 max-w-full items-center rounded-full border border-border bg-bg px-3 py-1 text-left text-sm active:bg-surface-2 pointer-coarse:min-h-11"
              >
                <span className="min-w-0 break-words">{r}</span>
              </button>
            ))}
          </div>
          <div className="flex items-center gap-2 pt-1 text-2xs text-faint">
            <span className="min-w-0 flex-1">{t.replies.hint}</span>
            <Button size="sm" variant="ghost" icon={<Sparkles />} onClick={start}>
              {t.replies.again}
            </Button>
          </div>
        </div>
      )}
      {replies && replies.length === 0 && state.phase === 'idle' && (
        <div className="flex items-center gap-2 text-xs text-muted">
          <span className="min-w-0 flex-1">{t.replies.none}</span>
          <Button size="sm" variant="ghost" icon={<Sparkles />} onClick={start}>
            {t.replies.again}
          </Button>
        </div>
      )}
      {!replies && state.phase === 'idle' && (
        <Button size="sm" variant="outline" icon={<Sparkles />} onClick={start}>
          {t.replies.suggest}
        </Button>
      )}
      <AssistPanel flow={flow} state={state} onRetry={start} />
    </div>
  );
}
