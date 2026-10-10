// The assistant request flow an app drives (`assistant.generate` -> preview -> `assistant.confirm`
// -> poll `assistant.get`). The preview shows the model, the estimated cost and the notice, and
// the user confirms it. Nothing is confirmed on the user's behalf: only a host that starts the
// request without asking for confirmation (it auto-sends this operation) skips the step.
// Pure state functions plus a small controller; the sheet is components/assist-flow.tsx.

import { RpcError, type AppApi, type AssistantPreview, type AssistantRequest, type HostConnectionApi } from '@vibeke/core';
import { t } from '../i18n';
import { ValueStore } from './store';

export type AssistPhase = 'idle' | 'starting' | 'confirm' | 'running' | 'done' | 'failed';
export type GenerateParams = AppApi['assistant.generate']['params'];
export type GenerateResult = AppApi['assistant.generate']['result'];

export interface AssistState {
  phase: AssistPhase;
  request: AssistantRequest | null;
  /** What would be sent: shown while `phase` is `confirm`, kept afterwards for the receipt. */
  preview: AssistantPreview | null;
  output: Record<string, unknown> | null;
  error: string | null;
  /** The failure is the missing assistant consent (given at the desk). */
  consent: boolean;
  /** A host remark on the request (`note`), e.g. why it was deduplicated. */
  note: string | null;
  /** The answer came from the host's cache: nothing was sent. */
  cached: boolean;
}

export const IDLE_ASSIST: AssistState = { phase: 'idle', request: null, preview: null, output: null, error: null, consent: false, note: null, cached: false };

const FAILED = new Set(['failed', 'cancelled', 'interrupted']);

/** The state a request record puts the flow in. */
export function stateFromRequest(req: AssistantRequest, base: AssistState = IDLE_ASSIST): AssistState {
  const keep = { ...base, request: req };
  if (req.state === 'done') return { ...keep, phase: 'done', output: req.output ?? {}, error: null };
  if (FAILED.has(req.state)) {
    const msg = req.error?.message?.trim();
    return { ...keep, phase: 'failed', output: null, error: msg || (req.state === 'cancelled' ? t.assist.cancelled : t.assist.failed) };
  }
  if (req.state === 'awaiting_confirmation') return { ...keep, phase: base.preview ? 'confirm' : 'failed', error: base.preview ? null : t.assist.noPreview };
  return { ...keep, phase: 'running', error: null };
}

/** The state `assistant.generate` puts the flow in. */
export function stateFromGenerate(res: GenerateResult): AssistState {
  const base: AssistState = { ...IDLE_ASSIST, preview: res.preview ?? null, note: res.note ?? null, cached: !!res.cached };
  if (res.requires_confirmation || res.request.state === 'awaiting_confirmation') {
    // Without a preview there is nothing to confirm against: stop rather than confirm blind.
    if (!res.preview) return { ...base, request: res.request, phase: 'failed', error: t.assist.noPreview };
    return { ...base, request: res.request, phase: 'confirm' };
  }
  return stateFromRequest(res.request, base);
}

const CONSENT_KINDS = new Set(['forbidden', 'permission', 'permission_denied', 'consent_required']);

/** A readable failure; `consent` when the host needs the assistant allowed for the workspace first. */
export function assistError(e: unknown): { message: string; consent: boolean } {
  if (e instanceof RpcError) {
    if (e.kind === 'method_not_found' || e.code === -32601) return { message: t.assist.unsupported, consent: false };
    if (CONSENT_KINDS.has(e.kind) || /consent|permission/i.test(e.message)) return { message: t.assist.noConsent, consent: true };
    const m = e.message.includes(': ') ? e.message.split(': ').slice(1).join(': ') : e.message;
    return { message: m, consent: false };
  }
  return { message: e instanceof Error ? e.message : String(e), consent: false };
}

/** Estimated cost as text: `about $0.02`, or null when the host did not estimate one. */
export function costText(p: AssistantPreview | null): string | null {
  const c = p?.estimated_max_cost_usd;
  if (typeof c !== 'number' || !Number.isFinite(c)) return null;
  return t.assist.cost(c < 0.01 ? '< $0.01' : `$${c.toFixed(2)}`);
}

export interface AssistConn {
  request: HostConnectionApi['request'];
}

export interface AssistOptions {
  sleep?(ms: number): Promise<void>;
  pollMs?: number;
  /** Stop polling after this long and report a timeout. */
  maxMs?: number;
  now?(): number;
}

/** One assistant request at a time: start, confirm, cancel, with stale answers dropped. */
export class AssistFlow {
  readonly store = new ValueStore<AssistState>(IDLE_ASSIST);
  private gen = 0;
  private readonly sleep: (ms: number) => Promise<void>;
  private readonly pollMs: number;
  private readonly maxMs: number;
  private readonly now: () => number;

  constructor(
    private readonly conn: () => AssistConn | undefined,
    o: AssistOptions = {},
  ) {
    this.sleep = o.sleep ?? ((ms) => new Promise((r) => setTimeout(r, ms)));
    this.pollMs = o.pollMs ?? 1000;
    this.maxMs = o.maxMs ?? 120_000;
    this.now = o.now ?? Date.now;
  }

  get state(): AssistState {
    return this.store.get();
  }

  async start(params: GenerateParams): Promise<void> {
    const g = ++this.gen;
    this.store.set({ ...IDLE_ASSIST, phase: 'starting' });
    try {
      const c = this.conn();
      if (!c) throw new Error(t.assist.offline);
      const res = await c.request('assistant.generate', params);
      if (g !== this.gen) return;
      this.store.set(stateFromGenerate(res));
      if (this.state.phase === 'running') await this.poll(g);
    } catch (e) {
      this.fail(g, e);
    }
  }

  /** The user confirmed the preview they saw. */
  async confirm(): Promise<void> {
    const s = this.state;
    if (s.phase !== 'confirm' || !s.request || !s.preview) return;
    const g = this.gen;
    this.store.set({ ...s, phase: 'running' });
    try {
      const c = this.conn();
      if (!c) throw new Error(t.assist.offline);
      const r = await c.request('assistant.confirm', { request: s.request.id, preview_digest: s.preview.digest });
      if (g !== this.gen) return;
      this.store.set(stateFromRequest(r.request, this.state));
      if (this.state.phase === 'running') await this.poll(g);
    } catch (e) {
      this.fail(g, e);
    }
  }

  /** Stop waiting; a request the host already holds is cancelled (best effort). */
  cancel(): void {
    const s = this.state;
    this.gen++;
    if (s.request && (s.phase === 'confirm' || s.phase === 'running')) {
      void this.conn()
        ?.request('assistant.cancel', { request: s.request.id })
        .catch(() => {});
    }
    this.store.set(IDLE_ASSIST);
  }

  reset(): void {
    this.gen++;
    this.store.set(IDLE_ASSIST);
  }

  private async poll(g: number): Promise<void> {
    const deadline = this.now() + this.maxMs;
    while (g === this.gen) {
      await this.sleep(this.pollMs);
      if (g !== this.gen) return;
      try {
        const id = this.state.request?.id;
        const c = this.conn();
        if (!id || !c) throw new Error(t.assist.offline);
        const r = await c.request('assistant.get', { request: id });
        if (g !== this.gen) return;
        this.store.set(stateFromRequest(r.request, this.state));
        if (this.state.phase !== 'running') return;
      } catch (e) {
        this.fail(g, e);
        return;
      }
      if (this.now() > deadline) {
        this.fail(g, new Error(t.assist.timeout));
        return;
      }
    }
  }

  private fail(g: number, e: unknown): void {
    if (g !== this.gen) return;
    const { message, consent } = assistError(e);
    this.store.set({ ...this.state, phase: 'failed', error: message, consent, output: null });
  }
}

// ---- reading outputs -------------------------------------------------------------------------

const isObj = (v: unknown): v is Record<string, unknown> => typeof v === 'object' && v !== null && !Array.isArray(v);

/** `reply_suggestions` output -> up to five distinct one-line replies. */
export function parseReplies(output: Record<string, unknown> | null): string[] {
  const list = output?.replies;
  if (!Array.isArray(list)) return [];
  const out: string[] = [];
  for (const r of list) {
    if (typeof r !== 'string') continue;
    const line = r.replace(/\s+/g, ' ').trim();
    if (line && !out.includes(line)) out.push(line);
    if (out.length === 5) break;
  }
  return out;
}

export interface SummaryItem {
  text: string;
  urgency?: 'now' | 'soon' | 'fyi';
}

/**
 * A briefing or background summary as items to show: `items[].text` (briefing), a `summary` /
 * `text` / `body` string, or a `summaries` list. Unknown shapes give nothing.
 */
export function summaryItems(output: Record<string, unknown> | null): SummaryItem[] {
  if (!output) return [];
  const out: SummaryItem[] = [];
  const push = (text: unknown, urgency?: unknown) => {
    if (typeof text !== 'string' || !text.trim()) return;
    out.push({ text: text.trim(), ...(urgency === 'now' || urgency === 'soon' || urgency === 'fyi' ? { urgency } : {}) });
  };
  for (const key of ['items', 'summaries']) {
    const list = output[key];
    if (!Array.isArray(list)) continue;
    for (const it of list) {
      if (typeof it === 'string') push(it);
      else if (isObj(it)) push(it.text ?? it.summary, it.urgency);
    }
  }
  if (!out.length) for (const key of ['summary', 'text', 'body']) push(output[key]);
  return out;
}
