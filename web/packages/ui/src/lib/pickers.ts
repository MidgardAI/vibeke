// Agent pickers and slash commands: the pure logic behind the picker card, the composer lock, the
// slash-command bar and the model switcher. Components stay thin; everything here is testable
// without a DOM.

import { RpcError, type AgentCommand, type AgentModel, type AppApi, type AppMethod, type Decision, type Interaction, type Question, type QuestionOption } from '@vibeke/core';
import { slashCommandsFor } from './harness';

// ---- picker interactions -----------------------------------------------------------------------

export const isPicker = (it: Pick<Interaction, 'kind'>): boolean => it.kind === 'picker';

/** A dialog the host could not parse: offer Cancel and the terminal only. */
export const isUnknownDialog = (it: Pick<Interaction, 'kind' | 'picker'>): boolean => it.kind === 'picker' && it.picker?.name === 'unknown';

/** The question that holds a picker's rows (`q0`). */
export const pickerQuestion = (it: Pick<Interaction, 'questions'>): Question | null => it.questions[0] ?? null;

/** Open pickers (and unknown dialogs) of a run or pane: while one exists the composer is locked. */
export function openDialogs<T extends { kind: string; status: string; run: string; pane: string }>(items: readonly T[], run: string | null | undefined, pane: string): T[] {
  return items.filter((i) => i.kind === 'picker' && i.status === 'open' && ((run && i.run === run) || i.pane === pane));
}

/** Option ids a multi-select starts from: the rows the agent shows as checked. */
export const initialChecked = (q: Pick<Question, 'options'>): string[] => q.options.filter((o) => o.selected).map((o) => o.id);

/** The row the agent points at (single-select), if any. */
export const currentOption = (q: Pick<Question, 'options'>): QuestionOption | null => q.options.find((o) => o.selected) ?? null;

/** Toggle one id in a set, keeping the order of the options. */
export function toggleId(options: readonly QuestionOption[], checked: readonly string[], id: string): string[] {
  const has = checked.includes(id);
  const next = has ? checked.filter((x) => x !== id) : [...checked, id];
  return options.map((o) => o.id).filter((x) => next.includes(x));
}

export interface PickerAnswer {
  decision?: Decision;
  choices?: Record<string, string[]>;
  expected_signature?: string;
}

const sig = (it: Pick<Interaction, 'picker'>): { expected_signature?: string } => (it.picker?.signature ? { expected_signature: it.picker.signature } : {});

/** Single-select: tapping a row answers with it. */
export const chooseAnswer = (it: Interaction, optionId: string): PickerAnswer => ({ choices: { [pickerQuestion(it)?.id ?? 'q0']: [optionId] }, ...sig(it) });

/** Multi-select: the full desired checked set. */
export const confirmAnswer = (it: Interaction, ids: readonly string[]): PickerAnswer => ({ choices: { [pickerQuestion(it)?.id ?? 'q0']: [...ids] }, ...sig(it) });

/** Left/right adjuster: the value to land on, under its own key. */
export const adjustAnswer = (it: Interaction, value: string): PickerAnswer => ({ choices: { adjust: [value] }, ...sig(it) });

/** Dismiss with the picker's cancel key. */
export const cancelAnswer = (it: Interaction): PickerAnswer => ({ decision: 'cancel', ...sig(it) });

export const canCancel = (it: Pick<Interaction, 'picker'>): boolean => !!it.picker?.cancel_key;

// ---- errors -----------------------------------------------------------------------------------

const detailsOf = (e: unknown): { reason?: string; interaction?: unknown } | null => {
  if (!(e instanceof RpcError)) return null;
  const d = e.data?.details;
  return d && typeof d === 'object' ? (d as { reason?: string; interaction?: unknown }) : {};
};

const idOf = (v: unknown): string | null => (typeof v === 'string' ? v : v && typeof v === 'object' && typeof (v as { id?: unknown }).id === 'string' ? (v as { id: string }).id : null);

/** `agent.prompt` refused because a picker or unknown dialog is open: `{interaction}` is its id when known. */
export function dialogOpen(e: unknown): { interaction: string | null } | null {
  if (!(e instanceof RpcError) || e.kind !== 'conflict') return null;
  const d = detailsOf(e);
  return d?.reason === 'dialog_open' ? { interaction: idOf(d.interaction) } : null;
}

/** `interaction.answer` refused because the dialog changed since the card was drawn. */
export function isPickerChanged(e: unknown): boolean {
  return e instanceof RpcError && e.kind === 'conflict' && detailsOf(e)?.reason === 'picker_changed';
}

/** The host does not know the method or the harness offers no structured way (older hosts included). */
export function isUnsupportedCall(e: unknown): boolean {
  return e instanceof RpcError && (e.kind === 'unsupported' || e.kind === 'method_not_found' || e.kind === 'not_found' || e.code === -32601);
}

// ---- prompt results ---------------------------------------------------------------------------

/** What a successful `agent.prompt` tells us: a picker it opened (focus it); no turn is fine. */
export function promptInteraction(r: unknown): string | null {
  const v = (r as { interaction?: unknown } | null | undefined)?.interaction;
  return typeof v === 'string' && v ? v : null;
}

// ---- slash commands ---------------------------------------------------------------------------

/** The built-in palette of a harness in the host's command shape (used when `agent.commands` is missing). */
export function fallbackCommands(harness: string | null | undefined): AgentCommand[] {
  return slashCommandsFor(harness).map((c) => ({ name: c.command, description: c.description, takes_arg: !!c.takesArg, opens_picker: false, dangerous: !!c.dangerous }));
}

/** Clean up a host's command list (a slash is always there, no duplicates). */
export function normalizeCommands(raw: unknown): AgentCommand[] {
  const list = (raw as { commands?: unknown } | null | undefined)?.commands;
  if (!Array.isArray(list)) return [];
  const seen = new Set<string>();
  const out: AgentCommand[] = [];
  for (const c of list as Partial<AgentCommand>[]) {
    if (!c || typeof c.name !== 'string' || !c.name.trim()) continue;
    const name = c.name.startsWith('/') ? c.name : `/${c.name}`;
    if (seen.has(name)) continue;
    seen.add(name);
    out.push({ name, description: c.description ?? '', takes_arg: !!c.takes_arg, opens_picker: !!c.opens_picker, dangerous: !!c.dangerous });
  }
  return out;
}

/** The slash word being typed: `/mo` → `mo`; null when the text is not a bare command prefix. */
export function slashQuery(text: string): string | null {
  const m = /^\/([^\s/]*)$/.exec(text);
  return m ? m[1]!.toLowerCase() : null;
}

/** Commands matching the typed prefix: names that start with it first, then names that contain it. */
export function filterCommands(cmds: readonly AgentCommand[], text: string): AgentCommand[] {
  const q = slashQuery(text);
  if (q === null) return [];
  const starts: AgentCommand[] = [];
  const has: AgentCommand[] = [];
  for (const c of cmds) {
    const n = c.name.slice(1).toLowerCase();
    if (n.startsWith(q)) starts.push(c);
    else if (q && (n.includes(q) || c.description.toLowerCase().includes(q))) has.push(c);
  }
  return [...starts, ...has];
}

export type CommandTap = { do: 'insert'; text: string } | { do: 'arm' } | { do: 'send'; text: string };

/** What tapping a command does: complete an argument, ask twice (dangerous), or just send it. */
export function commandTap(c: AgentCommand, armed: string | null): CommandTap {
  if (c.takes_arg && !c.opens_picker) return { do: 'insert', text: `${c.name} ` };
  if (c.dangerous && armed !== c.name) return { do: 'arm' };
  return { do: 'send', text: c.name };
}

type Req = { request<M extends AppMethod>(method: M, params: AppApi[M]['params']): Promise<AppApi[M]['result']> };

/**
 * Commands per host/run, loaded once. `agent.commands` failing (older host, unsupported harness,
 * offline) falls back to the built-in palette and is not retried until the run changes.
 */
export class CommandCache {
  private m = new Map<string, Promise<AgentCommand[]>>();
  private done = new Map<string, AgentCommand[]>();

  peek(key: string): AgentCommand[] | undefined {
    return this.done.get(key);
  }

  load(key: string, conn: Req | undefined, run: string, harness: string | null | undefined): Promise<AgentCommand[]> {
    const hit = this.m.get(key);
    if (hit) return hit;
    // Offline is worth another try later: show the built-in palette without remembering it.
    if (!conn) return Promise.resolve(fallbackCommands(harness));
    const p = (async () => {
      try {
        const list = normalizeCommands(await conn.request('agent.commands', { target: run }));
        return list.length ? list : fallbackCommands(harness);
      } catch {
        // Unsupported or failing: a fixed answer for this run.
        return fallbackCommands(harness);
      }
    })().then((l) => {
      if (this.m.get(key) === p) this.done.set(key, l);
      return l;
    });
    this.m.set(key, p);
    return p;
  }

  clear(): void {
    this.m.clear();
    this.done.clear();
  }
}

// ---- models -----------------------------------------------------------------------------------

export type ModelList = { kind: 'models'; models: AgentModel[] } | { kind: 'fallback' } | { kind: 'error'; message: string };

/** `agent.models`; unsupported (or missing on an older host) means: send `/model` instead. */
export async function loadModels(conn: Req, run: string): Promise<ModelList> {
  try {
    const r = await conn.request('agent.models', { target: run });
    const models = Array.isArray(r?.models) ? r.models : [];
    return models.length ? { kind: 'models', models } : { kind: 'fallback' };
  } catch (e) {
    if (isUnsupportedCall(e)) return { kind: 'fallback' };
    return { kind: 'error', message: e instanceof Error ? e.message : String(e) };
  }
}

/** `agent.set_model` refused because the harness saves every switch as its default model (pi). */
export function isPersistsDefault(e: unknown): boolean {
  return e instanceof RpcError && e.kind === 'conflict' && detailsOf(e)?.reason === 'persists_default';
}

/**
 * `agent.set_model` (for this session unless `scope` says otherwise); unsupported falls back to
 * the native `/model` picker. `confirm_default`: nothing changed because this harness would also
 * save the model as its default; ask the user and call again with `scope: 'default'`.
 */
export async function switchModel(
  conn: Req,
  send: (text: string) => Promise<boolean>,
  run: string,
  model: string,
  scope: 'session' | 'default' = 'session',
): Promise<'set' | 'picker' | 'failed' | 'confirm_default'> {
  try {
    await conn.request('agent.set_model', { target: run, model, scope });
    return 'set';
  } catch (e) {
    if (scope === 'session' && isPersistsDefault(e)) return 'confirm_default';
    if (isUnsupportedCall(e)) return (await send('/model')) ? 'picker' : 'failed';
    throw e;
  }
}
