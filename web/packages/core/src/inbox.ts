// Inbox ranking and batch grouping (spec 16 §7.6, §9.2). Pure functions over normalized models;
// the gateway re-checks batch eligibility at answer time, so this only decides what to offer.

import type { AgentRun, Decision, Interaction, Pane, Risk } from './model';

// ---- ranking -------------------------------------------------------------------------------

/**
 * Attention order of a risk badge. `unknown` sits with `high`: both always need an explicit
 * button + confirm (never swiped or batched), so they should not hide below routine approvals.
 * Interactions without an action (questions, plan reviews) rank as `unknown`: they block the agent.
 */
const RISK_RANK: Record<Risk, number> = { high: 0, unknown: 1, medium: 2, low: 3 };

export const interactionRisk = (i: Interaction): Risk => i.action?.risk ?? 'unknown';

/** Open interactions first by risk, then longest-waiting first. */
export function compareInteractions(a: Interaction, b: Interaction): number {
  return (
    RISK_RANK[interactionRisk(a)] - RISK_RANK[interactionRisk(b)] ||
    a.opened_at_ms - b.opened_at_ms ||
    a.id.localeCompare(b.id)
  );
}

export type Attention = 'interaction' | 'needs_input' | 'working' | 'idle';
const ATTENTION_RANK: Record<Attention, number> = { interaction: 0, needs_input: 1, working: 2, idle: 3 };

export interface AttentionContext {
  /** Per-run done_rev the user has already seen; a higher done_rev means "finished, look at me". */
  seenDoneRev?: (run: AgentRun) => number | undefined;
}

/** What a run asks of the user right now. */
export function runAttention(run: AgentRun, open: readonly Interaction[], ctx: AttentionContext = {}): Attention {
  if (open.some((i) => i.run === run.id && i.status === 'open')) return 'interaction';
  switch (run.execution.value) {
    case 'error':
    case 'rate_limited':
      return 'needs_input';
    case 'idle': {
      const seen = ctx.seenDoneRev?.(run);
      return seen !== undefined && run.done_rev > seen ? 'needs_input' : 'idle';
    }
    case 'working':
    case 'starting':
      return 'working';
    default:
      return 'idle';
  }
}

export interface RankedRun {
  run: AgentRun;
  attention: Attention;
  /** The run's most urgent open interaction, if any. */
  top: Interaction | null;
}

/**
 * Rank runs for the home list: open interaction (by its most urgent interaction's risk, then
 * wait), then needs input, working, idle; ties broken by most recent state change.
 */
export function rankRuns(runs: readonly AgentRun[], interactions: readonly Interaction[], ctx: AttentionContext = {}): RankedRun[] {
  const open = interactions.filter((i) => i.status === 'open');
  const byRun = new Map<string, Interaction>();
  for (const i of [...open].sort(compareInteractions)) if (!byRun.has(i.run)) byRun.set(i.run, i);
  return runs
    .map((run) => ({ run, attention: runAttention(run, open, ctx), top: byRun.get(run.id) ?? null }))
    .sort(
      (a, b) =>
        ATTENTION_RANK[a.attention] - ATTENTION_RANK[b.attention] ||
        (a.top && b.top ? compareInteractions(a.top, b.top) : 0) ||
        b.run.execution.since_ms - a.run.execution.since_ms ||
        a.run.id.localeCompare(b.run.id),
    );
}

/** An open interaction with where it came from (inbox spans hosts). */
export interface InboxItem {
  host_id: string;
  interaction: Interaction;
  run?: AgentRun;
  pane?: Pane;
}

/** Inbox cards: open interactions across hosts, risk first then wait time. */
export function rankInbox(items: readonly InboxItem[]): InboxItem[] {
  return items
    .filter((it) => it.interaction.status === 'open')
    .sort((a, b) => compareInteractions(a.interaction, b.interaction) || a.host_id.localeCompare(b.host_id));
}

/** Low/medium approvals may be swiped; high/unknown need button + confirm (§9.2). */
export const swipeAllowed = (i: Interaction): boolean => batchEligible(i);

// ---- batching ------------------------------------------------------------------------------

/** §7.6: approval, open, answerable, low/medium risk, with an action. */
export function batchEligible(i: Interaction): boolean {
  return (
    i.kind === 'approval' &&
    i.status === 'open' &&
    i.answerable &&
    i.action !== null &&
    (i.action.risk === 'low' || i.action.risk === 'medium')
  );
}

/** Never `allow_always` in a batch. */
export const batchDecisionAllowed = (d: Decision): d is 'allow' | 'deny' => d === 'allow' || d === 'deny';

/**
 * The command as it appears in the fingerprint: the exact string. Whitespace is never collapsed —
 * `echo harmless rm notes.txt` and `echo harmless\nrm notes.txt` are different commands and must
 * never be answered together (the gateway compares the exact string too).
 */
export const normalizeCommand = (cmd: string): string => cmd;

export interface FingerprintContext {
  /** Harness of the interaction's run. */
  harness: string;
  /** Repo root of the pane cwd (from git.status), falling back to the cwd itself. */
  repoRoot: string | null;
}

/**
 * Batch fingerprint (§7.6): (harness, action.tool, exact command or sorted paths, repo root).
 * Null when the interaction is not batch-eligible.
 */
export function batchFingerprint(i: Interaction, ctx: FingerprintContext): string | null {
  if (!batchEligible(i) || !i.action) return null;
  const target =
    i.action.command !== null && i.action.command.trim() !== ''
      ? `cmd:${normalizeCommand(i.action.command)}`
      : `paths:${JSON.stringify([...i.action.paths].sort())}`;
  return JSON.stringify([ctx.harness, i.action.tool, target, ctx.repoRoot ?? '']);
}

export interface Batch {
  fingerprint: string;
  host_id: string;
  items: InboxItem[];
  /** Highest risk in the group (low|medium). */
  risk: Risk;
}

/**
 * Group eligible inbox items by (host, fingerprint). Groups of one are not batches.
 * `repoRootOf` resolves the repo root for an item (default: run cwd, else pane cwd).
 */
export function groupBatches(
  items: readonly InboxItem[],
  repoRootOf: (item: InboxItem) => string | null = (it) =>
    it.interaction.repo_root ?? it.run?.cwd ?? it.pane?.cwd ?? null,
): Batch[] {
  const groups = new Map<string, Batch>();
  for (const it of rankInbox(items)) {
    // The gateway enriches interactions with `harness`; fall back to the run.
    const harness = it.interaction.harness ?? it.run?.harness;
    if (!harness) continue; // harness unknown → cannot fingerprint
    const fp = batchFingerprint(it.interaction, { harness, repoRoot: repoRootOf(it) });
    if (fp === null) continue;
    const key = `${it.host_id}\u0000${fp}`;
    let g = groups.get(key);
    if (!g) groups.set(key, (g = { fingerprint: fp, host_id: it.host_id, items: [], risk: 'low' }));
    g.items.push(it);
    if (it.interaction.action?.risk === 'medium') g.risk = 'medium';
  }
  return [...groups.values()].filter((g) => g.items.length > 1);
}

/** The interaction revision the user saw; the gateway requires it on every answer. */
export function decisionRev(it: Interaction): number {
  const rev = it.decision_rev;
  if (typeof rev !== 'number' || !Number.isInteger(rev) || rev < 0) throw new Error(`interaction ${it.id} has no decision_rev; refresh and try again`);
  return rev;
}

export interface AnswerFields {
  decision?: Decision;
  choices?: Record<string, string[]>;
  text?: string;
  expected_signature?: string;
}

/** Params for `interaction.answer`: always carries the `decision_rev` of the card the user saw. */
export function answerParams(it: Interaction, fields: AnswerFields) {
  const { decision, choices, text, expected_signature } = fields;
  return {
    interaction: it.id,
    ...(decision !== undefined ? { decision } : {}),
    ...(choices !== undefined ? { choices } : {}),
    ...(text !== undefined ? { text } : {}),
    ...(expected_signature !== undefined ? { expected_signature } : {}),
    decision_rev: decisionRev(it),
  };
}

/** Params for `interaction.answer_batch` (gateway re-validates every item, each with its `decision_rev`). */
export function batchAnswerParams(batch: Batch, decision: 'allow' | 'deny') {
  if (!batchDecisionAllowed(decision)) throw new Error('batch decision must be allow or deny');
  return {
    items: batch.items.map((it) => ({ interaction: it.interaction.id, decision_rev: decisionRev(it.interaction) })),
    decision,
  };
}
