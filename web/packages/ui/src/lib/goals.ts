// Goals on the phone: state wording and the rules for which buttons a goal shows.

import type { Goal, GoalStep } from '@vibeke/core';

export type GoalTone = 'default' | 'need' | 'add' | 'del' | 'info';

/** Chip tone per goal state. */
export function goalTone(state: string): GoalTone {
  switch (state) {
    case 'planned':
      return 'need';
    case 'approved':
    case 'running':
      return 'info';
    case 'done':
      return 'add';
    case 'failed':
      return 'del';
    default:
      return 'default';
  }
}

/** A goal in one of these states has nothing left to do. */
export const goalClosed = (state: string): boolean => state === 'done' || state === 'failed' || state === 'cancelled';

/** The plan waits for a decision: only then do Approve and Cancel show. */
export const planWaits = (g: Goal): boolean => g.state === 'planned' && !!g.plan;

export function planSteps(g: Goal): GoalStep[] {
  const steps = g.plan?.steps;
  return Array.isArray(steps) ? steps.filter((s) => s && typeof s === 'object') : [];
}

/** The step's own status text when the host sends one (`status` or `state`). */
export function stepStatus(s: GoalStep): string | null {
  const v = s.status ?? s.state;
  return typeof v === 'string' && v ? v : null;
}

/** 0..1, or null when the plan has no steps yet. */
export function progressFraction(p: { done: number; total: number }): number | null {
  if (!p || p.total <= 0) return null;
  return Math.min(1, Math.max(0, p.done / p.total));
}

/** Open goals first (planned ones, which wait for the user, on top), then closed ones. Stable otherwise. */
export function sortGoals<T extends { goal: Goal }>(list: readonly T[]): T[] {
  const rank = (g: Goal) => (g.state === 'planned' ? 0 : goalClosed(g.state) ? 2 : 1);
  return list.map((v, i) => ({ v, i })).sort((a, b) => rank(a.v.goal) - rank(b.v.goal) || a.i - b.i).map((x) => x.v);
}

/** A `stale` answer carries the current goal in `data.goal`; use it when it is a goal. */
export function goalFromStale(data: unknown): Goal | null {
  const g = (data as { goal?: unknown } | null | undefined)?.goal;
  return g && typeof g === 'object' && typeof (g as Goal).id === 'string' && typeof (g as Goal).state === 'string' ? (g as Goal) : null;
}

