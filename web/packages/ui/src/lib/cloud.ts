// Cloud sandboxes (spec 17): pure helpers for the stores, the send sheet and the Sandboxes screen.
// No React and no DOM, so they are unit tested directly.

import { needsAuth, type AppEvent, type CloudAuthMethod, type CloudBox, type CloudJob, type CloudJobState, type CloudProvider } from '@vibeke/core';

const str = (v: unknown): string | undefined => (typeof v === 'string' && v ? v : undefined);
const num = (v: unknown): number => (typeof v === 'number' && Number.isFinite(v) ? v : 0);
const obj = (v: unknown): Record<string, unknown> | null => (typeof v === 'object' && v !== null && !Array.isArray(v) ? (v as Record<string, unknown>) : null);

/** A job from the wire (a `cloud.job` event, or a `cloud.jobs` row); null when it has no id. */
export function parseCloudJob(raw: unknown, fallbackId?: string): CloudJob | null {
  const o = obj(raw);
  if (!o) return null;
  const id = str(o.id) ?? fallbackId;
  if (!id) return null;
  const progress = obj(o.progress);
  const error = obj(o.error);
  const result = obj(o.result);
  return {
    id,
    direction: o.direction === 'bring_back' ? 'bring_back' : 'send',
    pane: str(o.pane) ?? null,
    run: str(o.run) ?? null,
    box: str(o.box) ?? null,
    from: o.from,
    to: o.to,
    state: (str(o.state) ?? 'queued') as CloudJobState,
    progress: progress ? { done: num(progress.done), total: num(progress.total) } : null,
    error: error ? { kind: str(error.kind), message: str(error.message), details: error.details } : null,
    result: result
      ? {
          ...(str(result.pane) ? { pane: str(result.pane)! } : {}),
          ...(str(result.task) ? { task: str(result.task)! } : {}),
          ...(str(result.box) ? { box: str(result.box)! } : {}),
          ...(str(result.peer_job) ? { peer_job: str(result.peer_job)! } : {}),
        }
      : null,
    created_at: num(o.created_at),
    updated_at: num(o.updated_at),
  };
}

/** `cloud.job` carries the job as `data`, or as `data.job`. */
export function cloudJobFromEvent(e: AppEvent): CloudJob | null {
  if (e.type !== 'cloud.job') return null;
  const d = e.data as Record<string, unknown>;
  return parseCloudJob(obj(d.job) ?? d, e.subject.job);
}

/** `cloud.box.changed` carries the box view as `data`, or as `data.box`-keyed object. */
export function cloudBoxFromEvent(e: AppEvent): CloudBox | null {
  if (e.type !== 'cloud.box.changed') return null;
  const d = e.data as Record<string, unknown>;
  const raw = obj(d.box) ?? d;
  const box = str(raw.box) ?? (typeof d.box === 'string' ? d.box : undefined) ?? e.subject.box;
  if (!box) return null;
  return parseCloudBox({ ...raw, box });
}

export function parseCloudBox(raw: unknown): CloudBox | null {
  const o = obj(raw);
  const box = o && str(o.box);
  if (!o || !box) return null;
  const [provider, ...rest] = box.split('/');
  const unsynced = obj(o.unsynced);
  return {
    box,
    provider: str(o.provider) ?? provider ?? '',
    id: str(o.id) ?? rest.join('/'),
    name: str(o.name) ?? box,
    state: str(o.state) ?? 'unknown',
    ownership: (str(o.ownership) ?? 'attached') as CloudBox['ownership'],
    key: str(o.key),
    task: str(o.task) ?? null,
    workspace: str(o.workspace) ?? null,
    panes: Array.isArray(o.panes) ? o.panes.filter((p): p is string => typeof p === 'string') : [],
    sessions: num(o.sessions),
    created_at: num(o.created_at),
    last_activity_at: num(o.last_activity_at),
    url: str(o.url) ?? null,
    unsynced: unsynced
      ? { commits: num(unsynced.commits), dirty: num(unsynced.dirty), untracked: num(unsynced.untracked), summary: str(unsynced.summary) ?? '' }
      : null,
    caps: (Array.isArray(o.caps) || obj(o.caps) ? o.caps : {}) as CloudBox['caps'],
    host_tag: str(o.host_tag),
  };
}

/** Replace the job with the same id unless the copy we hold is newer. */
export function upsertCloudJob(list: readonly CloudJob[], j: CloudJob): CloudJob[] {
  const i = list.findIndex((x) => x.id === j.id);
  if (i < 0) return [j, ...list];
  const next = [...list];
  if (j.updated_at >= next[i]!.updated_at) next[i] = j;
  return next;
}

/** A destroyed box leaves the list. */
export function upsertCloudBox(list: readonly CloudBox[], b: CloudBox): CloudBox[] {
  const rest = list.filter((x) => x.box !== b.box);
  return b.state === 'destroyed' ? rest : [...rest, b];
}

export const cloudJobFinal = (j: CloudJob): boolean => j.state === 'done' || j.state === 'failed' || j.state === 'cancelled';

/** 0..100 when the job reports progress, else null (an indeterminate bar). */
export function cloudJobPct(j: CloudJob): number | null {
  const p = j.progress;
  if (!p || p.total <= 0) return null;
  return Math.max(0, Math.min(100, Math.round((p.done / p.total) * 100)));
}

export const cloudJobError = (j: CloudJob): string | null => (j.error ? (j.error.message ?? j.error.kind ?? 'failed') : null);

// ---- capabilities and actions --------------------------------------------------------------

/** Capabilities arrive as a flag map; a flag the provider does not mention counts as on. */
export const capOn = (caps: CloudBox['caps'] | CloudProvider['caps'], key: string): boolean => (Array.isArray(caps) ? caps.includes(key) : (caps as Record<string, boolean> | undefined)?.[key] !== false);

/** A capability that must be listed as on (checkpoints exist only where the provider says so). */
const capStrict = (caps: CloudBox['caps'], key: string): boolean => (Array.isArray(caps) ? caps.includes(key) : (caps as Record<string, boolean> | undefined)?.[key] === true);

export type BoxAction = 'open' | 'bring_back' | 'suspend' | 'resume' | 'checkpoint' | 'adopt' | 'forget' | 'destroy';

const RUNNING = new Set(['running', 'started', 'ready', 'warm']);
export const boxRunning = (b: CloudBox): boolean => RUNNING.has(b.state);
export const boxSuspended = (b: CloudBox): boolean => b.state === 'suspended' || b.state === 'paused' || b.state === 'cold';

/** Which row actions apply to a box (ownership first, then the provider's capabilities). */
export function boxActions(b: CloudBox): BoxAction[] {
  const out: BoxAction[] = [];
  if (b.ownership === 'missing') return ['forget'];
  if ((b.ownership === 'attached' || b.ownership === 'idle') && b.panes.length > 0) out.push('open', 'bring_back');
  if (b.ownership === 'orphaned' || b.ownership === 'foreign') out.push('adopt');
  if (boxRunning(b) && capOn(b.caps, 'explicit_suspend')) out.push('suspend');
  if (boxSuspended(b)) out.push('resume');
  if (boxRunning(b) && capStrict(b.caps, 'checkpoints')) out.push('checkpoint');
  out.push('destroy');
  return out;
}

export const hasUnsynced = (b: CloudBox): boolean => !!b.unsynced && b.unsynced.commits + b.unsynced.dirty + b.unsynced.untracked > 0;

/** `cloud.box.destroy` refused because the box holds work that is not on the host. */
export const isUnsyncedConflict = (e: unknown): boolean => {
  const x = e as { kind?: string; data?: { details?: { reason?: string } } } | null;
  return !!x && x.kind === 'conflict' && x.data?.details?.reason === 'unsynced_changes';
};

export interface ProviderGroup {
  provider: string;
  info: CloudProvider | null;
  boxes: CloudBox[];
}

/** Boxes grouped by provider (providers the host lists first, in order, then any others). */
export function groupBoxes(providers: readonly CloudProvider[], boxes: readonly CloudBox[]): ProviderGroup[] {
  const groups = new Map<string, ProviderGroup>();
  for (const p of providers) groups.set(p.id, { provider: p.id, info: p, boxes: [] });
  for (const b of boxes) {
    let g = groups.get(b.provider);
    if (!g) groups.set(b.provider, (g = { provider: b.provider, info: null, boxes: [] }));
    g.boxes.push(b);
  }
  for (const g of groups.values()) g.boxes.sort((a, b) => (b.last_activity_at ?? 0) - (a.last_activity_at ?? 0));
  return [...groups.values()];
}

/** The footer: running and idle counts. A running box without a live session counts as idle. */
export function boxCounts(boxes: readonly CloudBox[]): { running: number; idle: number } {
  let running = 0;
  let idle = 0;
  for (const b of boxes) {
    if (b.state === 'destroyed') continue;
    if (b.ownership === 'idle' || boxSuspended(b)) idle++;
    else if (boxRunning(b)) running++;
  }
  return { running, idle };
}

// ---- sign-in -------------------------------------------------------------------------------

/**
 * Run a cloud call. When it fails with `needs_auth`, ask `signIn` to authenticate (it resolves
 * true once the user signed in), then retry the call exactly once. Any other error, a refused
 * sign-in or a second `needs_auth` rejects with the error.
 */
export async function runWithCloudAuth<T>(call: () => Promise<T>, signIn: (provider: string, methods: CloudAuthMethod[]) => Promise<boolean>): Promise<T> {
  try {
    return await call();
  } catch (e) {
    const need = needsAuth(e);
    if (!need) throw e;
    if (!(await signIn(need.provider, need.methods))) throw e;
    return await call();
  }
}

/** How a provider's current sign-in reads: the `env:<VAR>` source, if any. */
export const authEnvVar = (p: CloudProvider): string | null => {
  const s = p.auth.source;
  return s && s.startsWith('env:') ? s.slice(4) : null;
};

// ---- destroying -----------------------------------------------------------------------------

export type DestroyOutcome = { k: 'destroyed' } | { k: 'unsynced'; unsynced: CloudBox['unsynced']; message: string };

interface DestroyConn {
  request(method: 'cloud.box.destroy', params: { box: string; force?: boolean }, opts?: { timeoutMs?: number }): Promise<{ box: string; destroyed: true }>;
}

/**
 * Destroy a box. Unsynced work without `force` is not an error for the caller: the outcome says
 * so, and the screen offers "Bring back first" or "Destroy anyway" (a second call with `force`).
 */
export async function tryDestroy(conn: DestroyConn, box: string, force = false): Promise<DestroyOutcome> {
  try {
    await conn.request('cloud.box.destroy', { box, ...(force ? { force: true } : {}) }, { timeoutMs: 120_000 });
    return { k: 'destroyed' };
  } catch (e) {
    if (!force && isUnsyncedConflict(e)) {
      const d = (e as { data?: { details?: { unsynced?: CloudBox['unsynced'] } } }).data?.details;
      return { k: 'unsynced', unsynced: d?.unsynced ?? null, message: (e as Error).message };
    }
    throw e;
  }
}
