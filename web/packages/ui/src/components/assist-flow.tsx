// The assistant request, shown inline: the preview to confirm (model, host, estimated cost and the
// host's notice), the wait, a failure with a plain reason, and the result through `children`.
// Shared by the catch-up cards and the suggested replies; any screen can reuse it.

import type { ReactNode } from 'react';
import { Bot } from 'lucide-react';
import type { AssistFlow, AssistState } from '../lib/assist-flow';
import { costText } from '../lib/assist-flow';
import { t } from '../i18n';
import { Button, Notice, Spinner } from './ui';

export function AssistPanel({
  flow,
  state,
  children,
  onRetry,
}: {
  flow: AssistFlow;
  state: AssistState;
  /** The finished output. */
  children?(output: Record<string, unknown>, s: AssistState): ReactNode;
  /** Offer a retry after a failure (not for a missing consent). */
  onRetry?(): void;
}) {
  switch (state.phase) {
    case 'idle':
      return null;
    case 'starting':
    case 'running':
      return (
        <div className="flex min-h-9 items-center gap-2 text-sm text-muted" role="status" aria-live="polite">
          <Spinner />
          {t.assist.working}
          <span className="flex-1" />
          <Button size="sm" variant="ghost" onClick={() => flow.cancel()}>
            {t.assist.cancel}
          </Button>
        </div>
      );
    case 'confirm':
      return <ConfirmCard flow={flow} state={state} />;
    case 'failed':
      return (
        <Notice
          tone={state.consent ? 'warn' : 'danger'}
          action={
            <>
              {onRetry && !state.consent && (
                <Button size="sm" variant="outline" onClick={onRetry}>
                  {t.retry}
                </Button>
              )}
              <Button size="sm" variant="ghost" onClick={() => flow.reset()}>
                {t.assist.close}
              </Button>
            </>
          }
        >
          {state.error ?? t.assist.failed}
        </Notice>
      );
    case 'done':
      return (
        <div className="space-y-1.5">
          {state.cached && <div className="text-2xs text-faint">{t.assist.cached}</div>}
          {children?.(state.output ?? {}, state)}
        </div>
      );
  }
}

function ConfirmCard({ flow, state }: { flow: AssistFlow; state: AssistState }) {
  const p = state.preview;
  if (!p) return null;
  const cost = costText(p);
  return (
    <div className="space-y-2 rounded-xl border border-border bg-surface-2 p-3" role="group" aria-label={t.assist.title}>
      <div className="flex items-center gap-2 text-sm font-medium">
        <Bot className="size-4 text-accent" aria-hidden />
        {t.assist.title}
      </div>
      <dl className="grid grid-cols-[auto_1fr] gap-x-3 gap-y-0.5 text-xs">
        {p.model && (
          <>
            <dt className="text-muted">{t.assist.model}</dt>
            <dd className="min-w-0 break-words">{p.model}</dd>
          </>
        )}
        {p.endpoint_host && (
          <>
            <dt className="text-muted">{t.assist.sentTo}</dt>
            <dd className="min-w-0 break-words">{p.endpoint_host}</dd>
          </>
        )}
        {typeof p.estimated_input_tokens === 'number' && (
          <>
            <dt className="text-muted">{t.assist.sizeLabel}</dt>
            <dd>{t.assist.tokens(p.estimated_input_tokens)}</dd>
          </>
        )}
        {cost && (
          <>
            <dt className="text-muted">{t.assist.costLabel}</dt>
            <dd>{cost}</dd>
          </>
        )}
      </dl>
      <p className="text-xs text-muted">{p.notice || t.assist.notice}</p>
      <div className="flex justify-end gap-2">
        <Button size="sm" variant="ghost" onClick={() => flow.cancel()}>
          {t.assist.cancel}
        </Button>
        <Button size="sm" variant="primary" onClick={() => void flow.confirm()}>
          {t.assist.confirm}
        </Button>
      </div>
    </div>
  );
}
