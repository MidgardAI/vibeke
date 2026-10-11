// "2 in background": the run's turn ended but the harness keeps tasks running.

import { Loader2 } from 'lucide-react';
import type { AgentRun } from '@vibeke/core';
import { bgSummary } from '../lib/bg-tasks';
import { Chip } from './ui';

export function BgChip({ run, className }: { run: AgentRun | null | undefined; className?: string }) {
  const text = bgSummary(run);
  if (!text) return null;
  return (
    <Chip tone="info" className={className} icon={<Loader2 className="animate-spin" />} title={text}>
      {text}
    </Chip>
  );
}
