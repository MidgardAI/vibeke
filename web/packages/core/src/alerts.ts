// Local notification content (spec 16 §7.8, §16.2), mirroring the gateway's push payloads
// (crates/vk-gateway/src/notify.rs) so a desktop notification reads like a phone push:
// one notification per host, merged ("3 agents need you"), shaped by the device privacy level.

import { redact } from './redact';
import { displayName, type AgentRun, type Dashboard, type Interaction } from './model';

export type PrivacyLevel = 'full' | 'summary' | 'minimal';

export interface AlertItem {
  /** Interaction id, or `run:<id>` for finished/stopped agents. */
  key: string;
  /** Full description, e.g. "Codex · samplehub wants to run `pnpm test`". */
  title: string;
  /** Hash route opened on click. */
  url: string;
  /** Needs an answer (interaction) rather than a heads-up (finished / stopped). */
  urgent: boolean;
}

export interface AlertPayload {
  title: string;
  body: string;
  url: string;
  count: number;
}

const capitalize = (s: string): string => (s ? s[0]!.toUpperCase() + s.slice(1) : s);

function truncate(s: string, n: number): string {
  const chars = [...s];
  return chars.length <= n ? s : `${chars.slice(0, n).join('')}…`;
}

/** Workspace display name of a run (via its pane). */
export function runWorkspace(d: Dashboard, run: AgentRun): string | null {
  const pane = d.panes.find((p) => p.id === run.pane);
  const ws = pane ? d.workspaces.find((w) => w.id === pane.workspace) : undefined;
  return ws ? displayName(ws) : null;
}

/** "Claude · backend" (or "An agent"). */
export function describeRun(d: Dashboard, run: AgentRun | undefined): string {
  if (!run) return 'An agent';
  const name = capitalize(run.harness || 'agent');
  const ws = runWorkspace(d, run);
  return ws ? `${name} · ${ws}` : name;
}

/** "Codex · samplehub wants to run `pnpm test`". */
export function describeInteraction(d: Dashboard, it: Interaction): string {
  const who = describeRun(
    d,
    d.runs.find((r) => r.id === it.run),
  );
  let what: string;
  switch (it.kind) {
    case 'approval':
      // Redact the whole command first: truncating first can cut a token below its pattern's
      // minimum length or drop the closing quote an assignment pattern needs.
      what = it.action?.command ? `wants to run \`${truncate(redact(it.action.command), 80)}\`` : it.action?.tool ? `wants to use ${redact(it.action.tool)}` : 'needs approval';
      break;
    case 'question':
      what = 'has a question';
      break;
    case 'plan_review':
      what = 'wants a plan reviewed';
      break;
    default:
      what = 'needs you';
  }
  return `${who} ${what}`;
}

/** `summary` level: keep "Codex · samplehub" and drop the command itself. */
export function stripCommand(t: string): string {
  const i = t.indexOf('`');
  if (i < 0) return t;
  return `${t.slice(0, i).trimEnd().replace(/ wants to run$/, '')} needs approval`;
}

/** One merged notification for a host's open items at the device's privacy level. */
export function alertPayload(items: readonly AlertItem[], privacy: PrivacyLevel, hostName: string): AlertPayload | null {
  const n = items.length;
  if (n === 0) return null;
  const first = items[0]!;
  const many = `${n} agents need you`;
  let title: string;
  let body: string;
  if (privacy === 'minimal') {
    title = 'Vibeke';
    body = n === 1 ? '1 agent needs you' : many;
  } else if (n === 1) {
    title = privacy === 'full' ? redact(first.title) : stripCommand(first.title);
    body = hostName;
  } else {
    title = many;
    body = hostName;
  }
  return { title, body, url: n === 1 ? first.url : '#/inbox', count: n };
}

export const isPrivacyLevel = (v: unknown): v is PrivacyLevel => v === 'full' || v === 'summary' || v === 'minimal';
