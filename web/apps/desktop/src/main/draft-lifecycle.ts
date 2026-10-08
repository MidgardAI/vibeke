import type { DraftStore } from './drafts';
import type { Engine } from './engine';

// Startup and re-pairing publish intermediate connection lists. Wait for startup and
// read the current full snapshot in a microtask, after synchronous detach/attach completes.
export async function syncDraftHosts(engine: Pick<Engine, 'start' | 'snapshot'>, drafts: DraftStore): Promise<void> {
  await engine.start();
  await drafts.retainHosts(engine.snapshot().map((s) => s.record.host_id));
}
