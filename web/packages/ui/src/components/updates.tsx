import { useState, useSyncExternalStore } from 'react';
import { ArrowDownToLine } from 'lucide-react';
import { useApp } from '../app/hooks';
import { emitUi } from '../app/keyboard';
import { Button, Notice, Sheet, Toggle } from './ui';
import type { UpdateState } from '../platform';

const absent: UpdateState = { status: 'idle', currentVersion: '' };
const noop = () => () => {};
export function useUpdates() {
  const capability = useApp().platform.updates;
  const state = useSyncExternalStore(capability?.subscribe ?? noop, capability?.get ?? (() => absent));
  return { capability, state };
}
export function updateLabel(s: UpdateState): string | null {
  switch (s.status) {
    case 'available': return `Update available · v${s.version}`;
    case 'downloading': return `Downloading update… ${s.progress ?? 0}%`;
    case 'ready': return 'Restart and update';
    case 'error': return 'Update failed · Retry';
    default: return null;
  }
}
export function UpdateSidebar() {
  const { capability, state } = useUpdates();
  const label = updateLabel(state);
  if (!capability || !label) return null;
  return <div className="shrink-0 border-t border-border px-2 py-1.5">
    <button type="button" className="vk-focus flex min-h-9 w-full items-center gap-2 rounded-md px-2 text-left text-xs text-accent hover:bg-hover" onClick={() => emitUi('updates')}>
      <ArrowDownToLine className="size-4 shrink-0" /><span aria-live="polite">{label}</span>
    </button>
  </div>;
}
export function UpdateControls() {
  const app = useApp();
  const { capability: u, state: s } = useUpdates();
  const [error, setError] = useState<string | null>(null);
  if (!u) return null;
  const run = (f: () => Promise<void>) => { setError(null); void f().catch((e) => setError((e as Error).message)); };
  const busy = s.status === 'checking' || s.status === 'downloading';
  return <div className="space-y-3" data-testid="desktop-updates">
    <p className="text-sm">Desktop app · v{s.currentVersion}</p>
    <p className="text-sm" role="status">{s.status === 'checking' ? 'Checking for updates…' : s.status === 'up-to-date' ? 'You’re up to date.' : updateLabel(s) ?? 'Check for a new desktop release.'}</p>
    {(error || s.message) && <Notice>{error || s.message}</Notice>}
    {s.manualReason && s.status === 'available' && <p className="text-xs text-muted">{s.manualReason}</p>}
    {s.status === 'downloading' && <progress className="w-full" aria-label="Update download" max={100} value={s.progress ?? 0} />}
    <div className="flex flex-wrap gap-2">
      {s.status === 'available' && (s.manualReason
        ? <Button onClick={() => s.downloadUrl && app.platform.openExternal(s.downloadUrl)}>Download installer</Button>
        : <Button onClick={() => run(u.download)}>Download update</Button>)}
      {s.status === 'ready' && <Button onClick={() => run(u.install)}>Restart and update</Button>}
      {!busy && s.status !== 'ready' && <Button variant="outline" onClick={() => run(u.check)}>{s.status === 'error' ? 'Retry' : 'Check for updates'}</Button>}
      {s.releaseUrl && <Button variant="ghost" onClick={() => app.platform.openExternal(s.releaseUrl!)}>Release notes</Button>}
    </div>
    {s.status === 'ready' && <p className="text-xs text-muted">Vibeke will reopen and reconnect to your hosts. On macOS, a downloaded update also installs when you next quit the app.</p>}
    <div className="flex items-center justify-between gap-3 text-sm"><span>Check automatically</span><Toggle label="Check for updates automatically" checked={s.automatic !== false} onChange={(value) => run(() => u.setAutomatic(value))} /></div>
    <p className="text-xs text-muted">This updates the desktop app. Update a host’s CLI from its terminal interface.</p>
  </div>;
}
export function UpdateSheet({ open, onClose }: { open: boolean; onClose(): void }) {
  if (!useApp().platform.updates) return null;
  return <Sheet open={open} onClose={onClose} title="Update Vibeke"><UpdateControls /><div className="mt-4"><Button variant="ghost" onClick={onClose}>Later</Button></div></Sheet>;
}
