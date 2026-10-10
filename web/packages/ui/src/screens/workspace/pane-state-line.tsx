// A one-line stand-in for the tab strip while the soft keyboard is open: which tab, and its
// status word (working, needs you, idle…).

import { StatusDot } from '../../components/ui';
import { stateTone, stateWord } from '../../components/pane-row';
import type { PaneRow } from '../../lib/tree';
import { paneStatus } from './tab-strip';

export function PaneStateLine({ row, label }: { row: PaneRow; label: string }) {
  const status = paneStatus(row);
  return (
    <div className="flex h-7 shrink-0 items-center gap-2 border-b border-border px-3 text-xs text-muted" data-keyboard-status>
      {status && <StatusDot status={status} />}
      <span className="min-w-0 flex-1 truncate">{label}</span>
      <span className="shrink-0" data-tone={stateTone(row)}>
        {stateWord(row)}
      </span>
    </div>
  );
}
