import { useState } from 'react';
import { Check, ChevronDown, ChevronUp, Layers, X } from 'lucide-react';
import type { Batch } from '@vibeke/core';
import { useApp, useHost } from '../app/hooks';
import { t } from '../i18n';
import { basename } from '../lib/format';
import { harnessLabel } from '../lib/harness';
import { InteractionCard } from './interaction-card';
import { Button, Card, Notice, RiskBadge, cx } from './ui';

/** "4 agents want `pnpm test` in samplehub" → Allow all / Deny all / expand (spec 16 §9.2). */
export function BatchCard({ batch, showHost, variant = 'default' }: { batch: Batch; showHost: boolean; variant?: 'default' | 'compact' }) {
  const app = useApp();
  const host = useHost(batch.host_id);
  const [expanded, setExpanded] = useState(false);
  const [busy, setBusy] = useState<'allow' | 'deny' | null>(null);
  const [result, setResult] = useState<string | null>(null);
  const first = batch.items[0]!.interaction;
  const what = first.action?.command ? `\`${first.action.command}\`` : (first.action?.paths.join(', ') ?? first.title);
  const repo = first.repo_root ?? batch.items[0]!.run?.cwd ?? '';
  const scope = host?.info?.scope ?? host?.record.scope;
  const can = host?.status === 'online' && scope !== 'view';

  const run = async (d: 'allow' | 'deny') => {
    setBusy(d);
    app.haptic('tap');
    const r = await app.answerBatch(batch, d);
    setBusy(null);
    if (r.ok !== r.total) setResult(t.inbox.batchPartial(r.ok, r.total));
  };

  if (expanded) {
    return (
      <div className="space-y-2">
        <button type="button" className="flex items-center gap-1 px-1 text-sm text-accent" onClick={() => setExpanded(false)}>
          <ChevronUp className="size-4" /> {t.inbox.collapse}
        </button>
        {batch.items.map((it) => (
          <InteractionCard key={it.interaction.id} item={it} showHost={showHost} variant={variant} />
        ))}
      </div>
    );
  }

  return (
    <div data-nav-item={`batch:${batch.host_id}:${batch.fingerprint}`} tabIndex={-1} aria-label={t.inbox.batchTitle(batch.items.length, '')}>
    <Card className={cx('animate-in space-y-2.5', variant === 'compact' ? 'rounded-xl p-3' : 'p-3.5')}>
      <div className="flex items-center gap-2 text-xs text-muted">
        <Layers className="size-3.5" />
        <span className="min-w-0 flex-1 truncate">
          {[harnessLabel(first.harness ?? batch.items[0]!.run?.harness), showHost ? (host?.info?.host_name ?? host?.record.name) : null].filter(Boolean).join(' · ')}
        </span>
        <RiskBadge risk={batch.risk} />
      </div>
      <div className={cx('font-medium leading-snug', variant === 'compact' ? 'text-sm' : 'text-base')}>
        {t.inbox.batchTitle(batch.items.length, '')}
        <code className="ml-1 rounded bg-surface-2 px-1 font-mono text-sm">{what.replace(/^`|`$/g, '')}</code>
        {repo && <span className="text-muted"> {t.inbox.batchIn(basename(repo))}</span>}
      </div>
      {result && <Notice tone="warn">{result}</Notice>}
      <div className="flex gap-2">
        <Button variant="outline" className="flex-1" disabled={!can || busy !== null} busy={busy === 'deny'} icon={<X className="size-4" />} data-act="deny" onClick={() => void run('deny')}>
          {t.inbox.denyAll}
        </Button>
        <Button variant="ok" className="flex-1" disabled={!can || busy !== null} busy={busy === 'allow'} icon={<Check className="size-4" />} data-act="allow" onClick={() => void run('allow')}>
          {t.inbox.allowAll}
        </Button>
      </div>
      <button type="button" data-act="open" className="flex items-center gap-1 text-sm text-accent" onClick={() => setExpanded(true)}>
        <ChevronDown className="size-4" /> {t.inbox.expand}
      </button>
    </Card>
    </div>
  );
}
