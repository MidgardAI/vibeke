// "While you were away": one card per workspace with activity since the user last looked. Built from
// what the app already knows (finished turns, open requests, check state, the last message) plus the
// workspace's changed files. With the assistant on, "Summarize" asks it for a short briefing through
// the preview and confirm step.

import { useEffect, useState } from 'react';
import { ArrowRight, Sparkles, X } from 'lucide-react';
import { useApp, useHost, useNow } from '../app/hooks';
import { t } from '../i18n';
import { summaryItems } from '../lib/assist-flow';
import { sumChanges, type CatchUpCard } from '../lib/catch-up';
import { ago } from '../lib/format';
import { useCatchUp } from '../lib/use-catch-up';
import { useAssistAvailable, useAssistFlow } from '../lib/use-assist';
import { useGitStatus } from '../lib/use-git-status';
import { navigate, workspaceRoute } from '../router';
import { AssistPanel } from './assist-flow';
import { Button, Chip, DiffCount, IconButton, cx } from './ui';

export function CatchUpSection({ catchUp, showHost }: { catchUp: ReturnType<typeof useCatchUp>; showHost: boolean }) {
  const { cards, dismiss, dismissAll } = catchUp;
  const now = useNow(30_000);
  if (!cards.length) return null;
  return (
    <section aria-label={t.catchUp.label} className="space-y-2">
      <div className="flex min-h-9 items-center gap-2 px-1">
        <h2 className="min-w-0 flex-1 truncate text-sm font-semibold">{t.catchUp.title}</h2>
        <Button size="sm" variant="ghost" onClick={dismissAll}>
          {t.catchUp.markAll}
        </Button>
      </div>
      {cards.map((c) => (
        <CatchUpCardView key={c.key} card={c} now={now} showHost={showHost} onDismiss={() => dismiss(c)} />
      ))}
    </section>
  );
}

function CatchUpCardView({ card, now, showHost, onDismiss }: { card: CatchUpCard; now: number; showHost: boolean; onDismiss(): void }) {
  const app = useApp();
  const host = useHost(card.host);
  const online = host?.status === 'online';
  const git = useGitStatus(online ? card.host : null, card.pane, { poll: false });
  const changes = git.status && !git.status.clean ? sumChanges(git.status.files) : null;
  const available = useAssistAvailable(card.host);
  const { flow, state } = useAssistFlow(card.host);
  const [shown, setShown] = useState(false);
  useEffect(() => {
    if (state.phase === 'idle') setShown(false);
  }, [state.phase]);
  const open = () => {
    app.haptic('tap');
    navigate(workspaceRoute(card.host, card.workspace, card.pane ? { pane: card.pane } : {}));
  };
  const summarize = () => {
    setShown(true);
    void flow.start({ operation: 'briefing', workspace: card.workspace });
  };
  return (
    <article className="space-y-2 rounded-2xl border border-border bg-surface p-3" data-catch-up={card.key}>
      <header className="flex items-start gap-2">
        <div className="min-w-0 flex-1">
          <div className="truncate text-sm font-medium">{card.title}</div>
          <div className="truncate text-xs text-muted">
            {[showHost ? card.hostName : null, card.branch ? `⎇ ${card.branch}` : null, t.catchUp.since(ago(card.since, now))].filter(Boolean).join(' · ')}
          </div>
        </div>
        <IconButton label={t.catchUp.dismiss} onClick={onDismiss} className="-mr-1 -mt-1">
          <X className="size-4" />
        </IconButton>
      </header>
      <div className="flex flex-wrap items-center gap-1.5">
        {card.turns > 0 && <Chip tone="info">{t.catchUp.turns(card.turns)}</Chip>}
        {card.runs.some((r) => r.isNew) && <Chip>{t.catchUp.newRun}</Chip>}
        {card.waiting > 0 && <Chip tone="need">{t.catchUp.waiting(card.waiting)}</Chip>}
        {card.checkLabel && <Chip>{`${t.catchUp.checks}: ${card.checkLabel}`}</Chip>}
        {changes && changes.files > 0 && (
          <Chip>
            {t.catchUp.files(changes.files)}
            <DiffCount adds={changes.adds} dels={changes.dels} />
          </Chip>
        )}
      </div>
      {card.lastMessage && (
        <p className="line-clamp-3 text-sm text-muted" title={t.catchUp.lastMessage}>
          {card.lastMessage}
        </p>
      )}
      {shown && (
        <AssistPanel flow={flow} state={state} onRetry={summarize}>
          {(output) => <Summary output={output} onClose={() => flow.reset()} />}
        </AssistPanel>
      )}
      <div className={cx('flex items-center gap-2', !shown && 'pt-0.5')}>
        <Button size="sm" variant="outline" icon={<ArrowRight />} onClick={open}>
          {t.catchUp.open}
        </Button>
        {available && state.phase === 'idle' && (
          <Button size="sm" variant="ghost" icon={<Sparkles />} onClick={summarize}>
            {t.catchUp.summarize}
          </Button>
        )}
      </div>
    </article>
  );
}

function Summary({ output, onClose }: { output: Record<string, unknown>; onClose(): void }) {
  const items = summaryItems(output);
  return (
    <div className="space-y-1.5 rounded-xl bg-surface-2 p-3 text-sm" aria-label={t.catchUp.summary}>
      {items.length === 0 && <p className="text-muted">{t.catchUp.noSummary}</p>}
      {items.length > 0 && (
        <ul className="space-y-1">
          {items.map((it, i) => (
            <li key={i} className={cx(it.urgency === 'now' && 'font-medium')}>
              {it.text}
            </li>
          ))}
        </ul>
      )}
      <div className="flex justify-end">
        <Button size="sm" variant="ghost" onClick={onClose}>
          {t.assist.close}
        </Button>
      </div>
    </div>
  );
}
