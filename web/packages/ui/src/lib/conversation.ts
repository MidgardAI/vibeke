// Conversation layout from transcript turns: user prompts, assistant text, and the steps in
// between (tool calls paired with their results, thinking). Long runs of steps fold into
// "N steps" so a busy turn reads as a few lines; the latest steps stay visible.

import type { TranscriptItem, TranscriptTurn } from '@vibeke/core';

export interface ToolStep {
  k: 'tool';
  key: string;
  call: TranscriptItem | null;
  result: TranscriptItem | null;
}

export interface ThinkingStep {
  k: 'thinking';
  key: string;
  text: string;
}

export type Step = ToolStep | ThinkingStep;

export type ConvBlock =
  | { k: 'user'; key: string; text: string }
  | { k: 'text'; key: string; text: string }
  | Step
  | { k: 'steps'; key: string; steps: Step[] };

/** Runs longer than this fold. */
export const FOLD_OVER = 5;
/** Steps left visible after a fold (the most recent ones). */
export const FOLD_KEEP = 3;

/**
 * User text cleaned for display: harness wrappers (`<command-name>/x</command-name>`, system
 * reminders, background-task notifications, local command output) collapse to what the user
 * typed. Empty → nothing to show.
 */
export function cleanUserText(text: string): string {
  let s = text.replace(/<(system-reminder|task-notification)>[\s\S]*?<\/\1>/g, '');
  const cmd = /<command-name>([\s\S]*?)<\/command-name>/.exec(s);
  if (cmd) {
    const args = /<command-args>([\s\S]*?)<\/command-args>/.exec(s);
    return `${cmd[1]!.trim()}${args && args[1]!.trim() ? ` ${args[1]!.trim()}` : ''}`;
  }
  s = s.replace(/<(local-command-stdout|local-command-stderr|local-command-caveat|command-message)>[\s\S]*?<\/\1>/g, '');
  return s.trim();
}

/** Blocks of one turn, before folding. */
export function turnSteps(turn: TranscriptTurn): ConvBlock[] {
  const out: ConvBlock[] = [];
  const byId = new Map<string, ToolStep>();
  let lastTool: ToolStep | null = null;
  turn.items.forEach((it, i) => {
    const key = `${turn.n}:${i}`;
    switch (it.kind) {
      case 'text': {
        const raw = it.text ?? '';
        if (it.role === 'user') {
          const text = cleanUserText(raw);
          if (text) out.push({ k: 'user', key, text });
        } else if (raw.trim()) out.push({ k: 'text', key, text: raw });
        lastTool = null;
        return;
      }
      case 'thinking': {
        const text = (it.text ?? it.summary ?? '').trim();
        if (text) out.push({ k: 'thinking', key, text });
        return;
      }
      case 'tool_call': {
        const step: ToolStep = { k: 'tool', key, call: it, result: null };
        if (it.id) byId.set(it.id, step);
        out.push(step);
        lastTool = step;
        return;
      }
      case 'tool_result': {
        const owner = (it.id ? byId.get(it.id) : undefined) ?? (lastTool && !lastTool.result && !it.id ? lastTool : undefined);
        if (owner && !owner.result) owner.result = it;
        else out.push({ k: 'tool', key, call: null, result: it });
        return;
      }
      default:
        if (it.text) out.push({ k: 'text', key, text: it.text });
    }
  });
  return out;
}

const isStep = (b: ConvBlock): b is Step => b.k === 'tool' || b.k === 'thinking';

/** Fold runs of more than FOLD_OVER consecutive steps: the older ones become one "N steps" block. */
export function foldSteps(blocks: ConvBlock[]): ConvBlock[] {
  const out: ConvBlock[] = [];
  let run: Step[] = [];
  const flush = () => {
    if (run.length > FOLD_OVER) {
      const hidden = run.slice(0, run.length - FOLD_KEEP);
      out.push({ k: 'steps', key: `steps:${hidden[0]!.key}`, steps: hidden });
      out.push(...run.slice(run.length - FOLD_KEEP));
    } else out.push(...run);
    run = [];
  };
  for (const b of blocks) {
    if (isStep(b)) run.push(b);
    else {
      flush();
      out.push(b);
    }
  }
  flush();
  return out;
}

export const turnBlocks = (turn: TranscriptTurn): ConvBlock[] => foldSteps(turnSteps(turn));

/** `45s`, `31m 46s`, `2h 5m`. */
export function workedFor(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  const h = Math.floor(m / 60);
  return `${h}h ${m % 60}m`;
}

export interface TurnStats {
  durationMs: number | null;
  tools: number;
  subagents: number;
}

const SUBAGENT_TOOLS = new Set(['Task', 'Agent', 'spawn_agent', 'spawn_subagent', 'create_agent']);

/** Timing and counts of a turn: the server's fields, else derived from the items. */
export function turnStats(turn: TranscriptTurn): TurnStats {
  const calls = turn.items.filter((i) => i.kind === 'tool_call');
  const tools = typeof turn.tool_count === 'number' ? turn.tool_count : calls.length;
  const subagents = typeof turn.subagent_count === 'number' ? turn.subagent_count : calls.filter((c) => SUBAGENT_TOOLS.has(c.tool ?? '')).length;
  let durationMs: number | null = typeof turn.duration_ms === 'number' ? turn.duration_ms : null;
  if (durationMs === null) {
    const ts = turn.items.map((i) => i.ts).filter((x): x is number => typeof x === 'number');
    if (ts.length >= 2) durationMs = Math.max(0, ts[ts.length - 1]! - ts[0]!);
  }
  return { durationMs, tools, subagents };
}

/** The assistant's words in a turn (Copy). */
export const turnText = (turn: TranscriptTurn): string =>
  turn.items
    .filter((i) => i.kind === 'text' && i.role !== 'user' && i.text?.trim())
    .map((i) => i.text!.trim())
    .join('\n\n');

/** Did the agent say or do anything in this turn (a footer is worth showing)? */
export const turnHasWork = (turn: TranscriptTurn): boolean => turn.items.some((i) => i.kind !== 'text' || i.role !== 'user');
