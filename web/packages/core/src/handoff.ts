// Handoff courier (spec 16 §15.2): the app carries an exported bundle from the source host to the
// destination in chunks (`handoff.read` → `handoff.begin`/`handoff.write`). Finishing is separate
// (`handoff.finish`, may answer `needs_repo`) and is never retried automatically: a lost finish
// result is "unknown, check the destination".

import type { AppApi, AppMethod, HandoffManifest } from './model';
import { RpcError, type RequestOptions } from './rpc';

/** What the courier needs from a host connection (HostConnection satisfies it). */
export interface ApiCaller {
  request<M extends AppMethod>(
    method: M,
    params: AppApi[M]['params'] & { op_id?: string },
    opts?: Omit<RequestOptions, 'mutating'>,
  ): Promise<AppApi[M]['result']>;
}

/** 2 MiB of payload per chunk (the gateway accepts ≤ 4 MiB). */
export const HANDOFF_CHUNK = 2 * 1024 * 1024;
const CHUNK_TIMEOUT_MS = 120_000;

export interface ExportedHandoff {
  id: string;
  size: number;
  sha256: string;
  manifest: HandoffManifest;
}

export class HandoffCancelled extends Error {
  constructor() {
    super('handoff cancelled');
    this.name = 'HandoffCancelled';
  }
}

/** A tiny cancellation token (AbortSignal-like, without needing DOM types in core). */
export interface CancelToken {
  readonly cancelled: boolean;
}

export interface TransferOptions {
  source: ApiCaller;
  dest: ApiCaller;
  exported: ExportedHandoff;
  chunk?: number;
  cancel?: CancelToken;
  onProgress?(sent: number, total: number): void;
}

/**
 * Copy the bundle source → destination. Resolves with the destination's handoff id (pass it to
 * `handoff.finish`). On cancel, both sides are discarded (best effort) and HandoffCancelled is
 * thrown; on any other failure the destination copy is discarded and the error rethrown, while
 * the source export is kept so the user can try again without exporting anew.
 */
export async function transferHandoff(o: TransferOptions): Promise<string> {
  const { source, dest, exported } = o;
  const chunk = Math.max(1, Math.min(o.chunk ?? HANDOFF_CHUNK, 4 * 1024 * 1024));
  const opts = { timeoutMs: CHUNK_TIMEOUT_MS };
  let destId: string | null = null;
  const checkCancel = async () => {
    if (!o.cancel?.cancelled) return;
    await discardHandoff(source, exported.id, dest, destId);
    throw new HandoffCancelled();
  };
  try {
    await checkCancel();
    const begun = await dest.request('handoff.begin', { manifest: exported.manifest, size: exported.size, sha256: exported.sha256 }, opts);
    destId = begun.id;
    let offset = 0;
    o.onProgress?.(0, exported.size);
    while (offset < exported.size) {
      await checkCancel();
      const r = await source.request('handoff.read', { id: exported.id, offset, len: chunk }, opts);
      if (r.size !== exported.size) throw new Error('the bundle changed on the source');
      if (!r.data_b64) throw new Error(`the source returned no data at offset ${offset}`);
      await checkCancel();
      const w = await dest.request('handoff.write', { id: destId, offset, data_b64: r.data_b64 }, opts);
      if (typeof w.received !== 'number' || w.received <= offset) throw new Error('the destination did not accept the chunk');
      offset = w.received;
      o.onProgress?.(offset, exported.size);
      if (r.eof && offset < exported.size) throw new Error('the source ended early');
    }
    return destId;
  } catch (e) {
    if (e instanceof HandoffCancelled) throw e;
    if (destId) await dest.request('handoff.discard', { id: destId }).catch(() => {});
    throw e;
  }
}

/** Best-effort cleanup of both ends (a handoff-invitation host may refuse `discard`; it expires in ≤ 1 h). */
export async function discardHandoff(source: ApiCaller | null, sourceId: string | null, dest: ApiCaller | null, destId: string | null): Promise<void> {
  await Promise.all([
    source && sourceId ? source.request('handoff.discard', { id: sourceId }).catch(() => {}) : undefined,
    dest && destId ? dest.request('handoff.discard', { id: destId }).catch(() => {}) : undefined,
  ]);
}

/**
 * `handoff.export` refused because the agent is working now (`busy`; older gateways said
 * `conflict` with a "working" message). The user may retry with `interrupt: true`.
 */
export const isHandoffBusy = (e: unknown): boolean =>
  e instanceof RpcError && (e.kind === 'busy' || (e.kind === 'conflict' && /working/i.test(e.message)));

export type ExportOutcome = { k: 'exported'; exported: ExportedHandoff } | { k: 'busy' };

/**
 * Export the pane's work on the source host. A working agent yields `{k:'busy'}` (offer
 * "Interrupt and hand off", which calls again with `interrupt`); other errors are thrown.
 */
export async function exportHandoff(source: ApiCaller, pane: string, interrupt = false): Promise<ExportOutcome> {
  try {
    const exported = await source.request('handoff.export', { pane, ...(interrupt ? { interrupt: true } : {}) }, { timeoutMs: 180_000 });
    return { k: 'exported', exported };
  } catch (e) {
    if (!interrupt && isHandoffBusy(e)) return { k: 'busy' };
    throw e;
  }
}

/** What the confirm screen shows before sending. */
export interface HandoffSummary {
  repo: string;
  origin: string | null;
  branch: string | null;
  harness: string | null;
  /** The destination can resume the same agent session (transcript + resume args present). */
  resumable: boolean;
  secrets: string[];
  otherSkipped: { path: string; reason: string }[];
  redactions: number;
  untracked: number;
  lastMessage: string | null;
}

export function handoffSummary(m: HandoffManifest): HandoffSummary {
  const skipped = m.skipped ?? [];
  return {
    repo: m.repo_name,
    origin: m.origin ?? null,
    branch: m.branch ?? null,
    harness: m.harness ?? null,
    resumable: !!(m.harness && m.transcript_rel && m.session_id && (m.resume_args?.length ?? 0) > 0),
    secrets: skipped.filter((s) => s.reason === 'secret').map((s) => s.path),
    otherSkipped: skipped.filter((s) => s.reason !== 'secret'),
    redactions: m.redactions ?? 0,
    untracked: m.untracked?.length ?? 0,
    lastMessage: m.last_message ?? null,
  };
}
